//! Writing synced items into the space's own tables.
//!
//! Everything a backend pulls lands in `space_events` / `space_notes`, the
//! same tables the space UI reads — there is no shadow copy of a calendar.
//! The remote's own id is kept in `link` so the next sync updates the row it
//! created instead of inserting a duplicate every run, which is the failure
//! that makes a sync look like it works and then fills a calendar with
//! triplicates.

use anyhow::Result;
use chrono::Utc;
use rusqlite::params;
use uuid::Uuid;

use crate::db::Db;

use super::ical::VEvent;
use super::SyncReport;

/// How a synced row is recognised again: `<source>:<remote id>` in `link`.
pub fn remote_key(source: &str, remote_id: &str) -> String {
    format!("{source}:{remote_id}")
}

/// Insert or update one event, keyed by its remote id.
///
/// A cancelled event is soft-deleted rather than dropped on the floor: the
/// space UI filters on `deleted_at`, so a meeting called off remotely
/// disappears locally instead of lingering as a ghost.
pub fn upsert_event(db: &Db, source: &str, e: &VEvent, report: &mut SyncReport) {
    let key = remote_key(source, &e.uid);
    let now = Utc::now().timestamp_millis();

    let outcome = db.with_conn(|conn| {
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM space_events WHERE link = ?1",
                params![key],
                |r| r.get(0),
            )
            .ok();

        if e.cancelled {
            if let Some(id) = &existing {
                conn.execute(
                    "UPDATE space_events SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
                return Ok(Outcome::Updated);
            }
            return Ok(Outcome::Skipped);
        }

        match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE space_events SET title = ?2, description = ?3, start_at = ?4,
                        end_at = ?5, all_day = ?6, location = ?7, recurrence = ?8,
                        source = ?9, updated_at = ?10, deleted_at = NULL
                     WHERE id = ?1",
                    params![
                        id,
                        e.summary,
                        e.description,
                        e.start_ms,
                        e.end_ms,
                        i64::from(e.all_day),
                        e.location,
                        e.rrule,
                        source,
                        now
                    ],
                )?;
                Ok(Outcome::Updated)
            }
            None => {
                conn.execute(
                    "INSERT INTO space_events
                       (id, title, description, start_at, end_at, all_day, location,
                        recurrence, link, source, status, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'upcoming', ?11, ?11)",
                    params![
                        Uuid::new_v4().to_string(),
                        e.summary,
                        e.description,
                        e.start_ms,
                        e.end_ms,
                        i64::from(e.all_day),
                        e.location,
                        e.rrule,
                        key,
                        source,
                        now
                    ],
                )?;
                Ok(Outcome::Created)
            }
        }
    });

    match outcome {
        Ok(Outcome::Created) => {
            report.synced += 1;
            report.created += 1;
        }
        Ok(Outcome::Updated) => {
            report.synced += 1;
            report.updated += 1;
        }
        Ok(Outcome::Skipped) => {}
        Err(e) => report.note_error(format!("write event: {e}")),
    }
}

/// Insert or update one note, keyed by its remote id.
pub fn upsert_note(
    db: &Db,
    source: &str,
    remote_id: &str,
    title: &str,
    body: &str,
    report: &mut SyncReport,
) {
    let key = remote_key(source, remote_id);
    let now = Utc::now().timestamp_millis();
    // The tag is what distinguishes a pulled note from one written here, and
    // it is the only place `space_notes` can carry provenance — the table has
    // no `link` column and adding one would migrate every install.
    let tags = serde_json::json!([source]).to_string();

    let outcome = db.with_conn(|conn| {
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM space_notes WHERE folder_id = ?1",
                params![key],
                |r| r.get(0),
            )
            .ok();
        match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE space_notes SET title = ?2, body = ?3, updated_at = ?4,
                       deleted_at = NULL WHERE id = ?1",
                    params![id, title, body, now],
                )?;
                Ok(Outcome::Updated)
            }
            None => {
                conn.execute(
                    "INSERT INTO space_notes
                       (id, title, body, tags, folder_id, pinned, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?6)",
                    params![Uuid::new_v4().to_string(), title, body, tags, key, now],
                )?;
                Ok(Outcome::Created)
            }
        }
    });

    match outcome {
        Ok(Outcome::Created) => {
            report.synced += 1;
            report.created += 1;
        }
        Ok(Outcome::Updated) => {
            report.synced += 1;
            report.updated += 1;
        }
        Ok(Outcome::Skipped) => {}
        Err(e) => report.note_error(format!("write note: {e}")),
    }
}

enum Outcome {
    Created,
    Updated,
    Skipped,
}

/// Where a backend's cursor lives between runs.
pub fn cursor_key(source: &str) -> String {
    format!("space:sync:{source}:cursor")
}

pub fn load_cursor(db: &Db, source: &str) -> Option<String> {
    db.get_router_state(&cursor_key(source))
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

pub fn save_cursor(db: &Db, source: &str, cursor: &str) -> Result<()> {
    db.set_router_state(&cursor_key(source), cursor)
}

/// Timestamp of the last run, for the "last synced" line in the UI.
pub fn record_run(db: &Db, source: &str, report: &SyncReport) {
    let value = serde_json::json!({
        "at": Utc::now().timestamp_millis(),
        "report": report,
    })
    .to_string();
    let _ = db.set_router_state(&format!("space:sync:{source}:last"), &value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Db {
        let mut cfg = crate::config::Config::from_env();
        let dir = std::env::temp_dir().join(format!("space-sync-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        cfg.paths.db_path = dir.join("test.db");
        // `Db::open` opens two files; leaving the cognitive path alone would
        // migrate the developer's real ~/.senclaw cognitive DB.
        cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
        Db::open(&cfg).unwrap()
    }

    fn event(uid: &str, summary: &str, start_ms: i64) -> VEvent {
        VEvent {
            uid: uid.into(),
            summary: summary.into(),
            description: None,
            location: None,
            start_ms,
            end_ms: start_ms + 3_600_000,
            all_day: false,
            rrule: None,
            cancelled: false,
        }
    }

    #[test]
    fn a_second_sync_updates_rather_than_duplicates() {
        // The failure this guards: every run inserting again, so a calendar
        // fills with copies while the report says the sync succeeded.
        let db = test_db();
        let mut r = SyncReport::default();
        upsert_event(&db, "google", &event("u1", "Standup", 1_000), &mut r);
        assert_eq!((r.created, r.updated), (1, 0));

        let mut r2 = SyncReport::default();
        upsert_event(&db, "google", &event("u1", "Standup moved", 2_000), &mut r2);
        assert_eq!((r2.created, r2.updated), (0, 1));

        let count: i64 = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT COUNT(*) FROM space_events", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(count, 1);
        let title: String = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT title FROM space_events", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(title, "Standup moved");
    }

    #[test]
    fn two_sources_with_the_same_uid_stay_separate() {
        let db = test_db();
        let mut r = SyncReport::default();
        upsert_event(&db, "google", &event("shared", "G", 1_000), &mut r);
        upsert_event(&db, "caldav", &event("shared", "C", 1_000), &mut r);
        let count: i64 = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT COUNT(*) FROM space_events", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn a_cancelled_event_is_soft_deleted_not_left_as_a_ghost() {
        let db = test_db();
        let mut r = SyncReport::default();
        upsert_event(&db, "google", &event("u1", "Meeting", 1_000), &mut r);
        let mut cancelled = event("u1", "Meeting", 1_000);
        cancelled.cancelled = true;
        upsert_event(&db, "google", &cancelled, &mut r);

        let deleted: Option<i64> = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT deleted_at FROM space_events", [], |r| r.get(0))?)
            })
            .unwrap();
        assert!(deleted.is_some(), "the space UI filters on deleted_at");
    }

    #[test]
    fn cancelling_an_event_we_never_saw_writes_nothing() {
        let db = test_db();
        let mut r = SyncReport::default();
        let mut cancelled = event("never", "Ghost", 1_000);
        cancelled.cancelled = true;
        upsert_event(&db, "google", &cancelled, &mut r);
        let count: i64 = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT COUNT(*) FROM space_events", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(r.synced, 0);
    }

    #[test]
    fn a_cursor_round_trips() {
        let db = test_db();
        assert!(load_cursor(&db, "google").is_none());
        save_cursor(&db, "google", "tok-1").unwrap();
        assert_eq!(load_cursor(&db, "google").as_deref(), Some("tok-1"));
    }

    #[test]
    fn notes_update_in_place_too() {
        let db = test_db();
        let mut r = SyncReport::default();
        upsert_note(&db, "apple-notes", "n1", "Title", "body", &mut r);
        upsert_note(&db, "apple-notes", "n1", "Title", "edited", &mut r);
        let (count, body): (i64, String) = db
            .with_conn(|c| {
                Ok(c.query_row("SELECT COUNT(*), MAX(body) FROM space_notes", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?)
            })
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(body, "edited");
    }

    #[test]
    fn repeated_errors_are_recorded_once() {
        let mut r = SyncReport::default();
        for _ in 0..100 {
            r.note_error("bad event");
        }
        assert_eq!(r.errors.len(), 1);
    }
}

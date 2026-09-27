//! Event notification loop.
//!
//! Polls `space_events` every `interval_sec` seconds and:
//! 1. Updates event `status` based on current time (upcoming → ongoing → done).
//! 2. Fires a reminder notification when `start_at - reminder_min*60s <= now`
//!    and `reminder_sent_at IS NULL`.
//! 3. Fires re-notifications every `renotify_min` minutes while `status = 'ongoing'`.
//!
//! Each fire is persisted to `event_notifications` and pushed via
//! `WebSocketGateway` as `space:event:reminder` WS events.

use std::sync::Arc;
use std::time::Duration;

use rusqlite::params;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use uuid::Uuid;

use crate::db::event_notifications::{insert_event_notification_conn, StoredEventNotification};
use crate::db::Db;
use crate::util::local_time::local_iso_string_now;

/// Callback seam so the notifier can push WS events without hard-coupling to
/// `WebSocketGateway` (keeps the scheduler crate independent).
pub trait EventNotifySink: Send + Sync + 'static {
    /// `notification_id` is the persisted DB row id, so the UI can dedupe
    /// across the live frame and the subscribe replay (and target the row
    /// when marking as read).
    /// `delayed_ms` is `now - trigger_time` — non-zero when the daemon was
    /// down past the moment the reminder should have fired.
    fn notify_event_reminder(
        &self,
        notification_id: &str,
        event_id: &str,
        title: &str,
        start_at_ms: i64,
        kind: &str,
        fired_at_ms: i64,
        delayed_ms: i64,
    );
}

/// No-op sink used when the WS gateway is not wired.
pub struct NoopEventSink;
impl EventNotifySink for NoopEventSink {
    fn notify_event_reminder(
        &self,
        _notification_id: &str,
        _event_id: &str,
        _title: &str,
        _start_at_ms: i64,
        _kind: &str,
        _fired_at_ms: i64,
        _delayed_ms: i64,
    ) {
    }
}

pub struct EventNotifier {
    db: Arc<Db>,
    sink: Arc<dyn EventNotifySink>,
    interval: Duration,
}

impl EventNotifier {
    pub fn new(db: Arc<Db>, sink: Arc<dyn EventNotifySink>, interval_sec: u64) -> Self {
        Self {
            db,
            sink,
            interval: Duration::from_secs(interval_sec.max(10)),
        }
    }

    pub fn start(self) -> JoinHandle<()> {
        let interval = self.interval;
        info!(interval_sec = interval.as_secs(), "[EventNotifier] started");
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if let Err(e) = self.tick() {
                    warn!(error = %e, "[EventNotifier] tick failed");
                }
            }
        })
    }

    fn persist_and_notify(
        &self,
        conn: &rusqlite::Connection,
        event_id: &str,
        title: &str,
        start_at: i64,
        kind: &str,
        trigger_at: i64,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        let notification_id = Uuid::new_v4().to_string();
        let delayed_ms = (now_ms - trigger_at).max(0);
        let notif = StoredEventNotification {
            id: notification_id.clone(),
            event_id: event_id.to_string(),
            title: title.to_string(),
            start_at,
            kind: kind.to_string(),
            fired_at: now_ms,
            delayed_ms,
            read_at: None,
        };
        insert_event_notification_conn(conn, &notif)?;
        self.sink.notify_event_reminder(
            &notification_id,
            event_id,
            title,
            start_at,
            kind,
            now_ms,
            delayed_ms,
        );
        Ok(())
    }

    pub fn tick(&self) -> anyhow::Result<()> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        self.db.with_conn(|conn| {
            // ── 1. Transition upcoming → ongoing / ongoing → done ────────────
            conn.execute(
                "UPDATE space_events
                 SET status = 'ongoing', updated_at = ?1
                 WHERE status = 'upcoming' AND deleted_at IS NULL
                   AND start_at <= ?1 AND end_at > ?1",
                params![now_ms],
            )?;
            conn.execute(
                "UPDATE space_events
                 SET status = 'done', updated_at = ?1
                 WHERE status IN ('upcoming','ongoing') AND deleted_at IS NULL
                   AND end_at <= ?1",
                params![now_ms],
            )?;

            // ── 2. Fire reminders ────────────────────────────────────────────
            let mut stmt = conn.prepare(
                "SELECT id, title, start_at, reminder_min
                 FROM space_events
                 WHERE deleted_at IS NULL
                   AND reminder_min IS NOT NULL
                   AND reminder_sent_at IS NULL
                   AND status IN ('upcoming','ongoing')
                   AND (start_at - reminder_min * 60000) <= ?1",
            )?;
            let reminders: Vec<(String, String, i64, i64)> = stmt
                .query_map(params![now_ms], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .collect();

            for (id, title, start_at, reminder_min) in reminders {
                let trigger_at = start_at - reminder_min * 60_000;
                info!(
                    "[EventNotifier] reminder → \"{}\" starts at {} (local: {}, delayed_ms={})",
                    title,
                    start_at,
                    local_iso_string_now(),
                    (now_ms - trigger_at).max(0)
                );
                self.persist_and_notify(
                    conn, &id, &title, start_at, "reminder", trigger_at, now_ms,
                )?;
                conn.execute(
                    "UPDATE space_events SET reminder_sent_at = ?1, updated_at = ?1 WHERE id = ?2",
                    params![now_ms, id],
                )?;
            }

            // ── 2.5 Fire "event is starting" notifications ───────────────────
            // Every event pings at its start time, regardless of whether a
            // reminder_min was configured. This is the baseline guarantee:
            // "notification khi đến thời điểm". `start_sent_at` makes it
            // exactly-once. Catches events the user created without an
            // explicit reminder (e.g. "thêm sự kiện Đi Uniqlo lúc 14h").
            let mut stmt = conn.prepare(
                "SELECT id, title, start_at
                 FROM space_events
                 WHERE deleted_at IS NULL
                   AND start_sent_at IS NULL
                   AND start_at <= ?1
                   AND end_at > ?1",
            )?;
            let starts: Vec<(String, String, i64)> = stmt
                .query_map(params![now_ms], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .collect();

            for (id, title, start_at) in starts {
                info!(
                    "[EventNotifier] start → \"{}\" is starting now (local: {}, delayed_ms={})",
                    title,
                    local_iso_string_now(),
                    (now_ms - start_at).max(0)
                );
                // trigger_at = start_at → delayed_ms measured from the
                // event's actual start moment.
                self.persist_and_notify(conn, &id, &title, start_at, "start", start_at, now_ms)?;
                conn.execute(
                    "UPDATE space_events SET start_sent_at = ?1, updated_at = ?1 WHERE id = ?2",
                    params![now_ms, id],
                )?;
            }

            // ── 3. Re-notifications for ongoing events ───────────────────────
            let mut stmt = conn.prepare(
                "SELECT id, title, start_at, renotify_min, renotify_sent_at
                 FROM space_events
                 WHERE deleted_at IS NULL
                   AND status = 'ongoing'
                   AND renotify_min IS NOT NULL
                   AND (renotify_sent_at IS NULL
                        OR (?1 - renotify_sent_at) >= renotify_min * 60000)",
            )?;
            let renotifies: Vec<(String, String, i64, i64, Option<i64>)> = stmt
                .query_map(params![now_ms], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .collect();

            for (id, title, start_at, renotify_min, renotify_sent_at) in renotifies {
                let trigger_at = renotify_sent_at
                    .map(|t| t + renotify_min * 60_000)
                    .unwrap_or(start_at);
                info!(
                    "[EventNotifier] re-notify → \"{}\" ongoing (local: {})",
                    title,
                    local_iso_string_now()
                );
                self.persist_and_notify(
                    conn, &id, &title, start_at, "renotify", trigger_at, now_ms,
                )?;
                conn.execute(
                    "UPDATE space_events SET renotify_sent_at = ?1, updated_at = ?1 WHERE id = ?2",
                    params![now_ms, id],
                )?;
            }

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Recorder(Mutex<Vec<(String, String, String, i64)>>);
    impl EventNotifySink for Recorder {
        fn notify_event_reminder(
            &self,
            notification_id: &str,
            event_id: &str,
            _title: &str,
            _start_at_ms: i64,
            kind: &str,
            _fired_at_ms: i64,
            delayed_ms: i64,
        ) {
            self.0.lock().unwrap().push((
                notification_id.to_string(),
                event_id.to_string(),
                kind.to_string(),
                delayed_ms,
            ));
        }
    }

    fn insert_event(
        conn: &rusqlite::Connection,
        id: &str,
        title: &str,
        start_ms: i64,
        end_ms: i64,
        reminder_min: Option<i64>,
        renotify_min: Option<i64>,
    ) {
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO space_events
             (id, title, start_at, end_at, reminder_min, renotify_min, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?7)",
            params![id, title, start_ms, end_ms, reminder_min, renotify_min, now],
        )
        .unwrap();
    }

    #[test]
    fn reminder_fires_when_due() {
        let cfg = crate::config::Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let now_ms = chrono::Utc::now().timestamp_millis();
        db.with_conn(|conn| {
            insert_event(
                conn,
                "e1",
                "Meeting",
                now_ms + 2 * 60_000,
                now_ms + 62 * 60_000,
                Some(5),
                None,
            );
            Ok(())
        })
        .unwrap();

        let rec = Arc::new(Recorder(Mutex::new(vec![])));
        let notifier = EventNotifier::new(db.clone(), rec.clone(), 60);
        notifier.tick().unwrap();

        let fired = rec.0.lock().unwrap().clone();
        assert!(fired
            .iter()
            .any(|(_, id, kind, _)| id == "e1" && kind == "reminder"));

        let rows = db.list_event_notifications(None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_id, "e1");
        assert_eq!(rows[0].kind, "reminder");
    }

    #[test]
    fn reminder_not_fired_twice() {
        let cfg = crate::config::Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let now_ms = chrono::Utc::now().timestamp_millis();
        db.with_conn(|conn| {
            insert_event(
                conn,
                "e2",
                "Standup",
                now_ms + 60_000,
                now_ms + 120_000,
                Some(10),
                None,
            );
            Ok(())
        })
        .unwrap();

        let rec = Arc::new(Recorder(Mutex::new(vec![])));
        let notifier = EventNotifier::new(db, rec.clone(), 60);
        notifier.tick().unwrap();
        notifier.tick().unwrap();

        let count = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, id, _, _)| id == "e2")
            .count();
        assert_eq!(count, 1, "reminder should fire exactly once");
    }

    #[test]
    fn status_transitions_upcoming_to_done() {
        let cfg = crate::config::Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let now_ms = chrono::Utc::now().timestamp_millis();
        db.with_conn(|conn| {
            insert_event(
                conn,
                "e3",
                "Past",
                now_ms - 120_000,
                now_ms - 60_000,
                None,
                None,
            );
            Ok(())
        })
        .unwrap();

        let notifier = EventNotifier::new(db.clone(), Arc::new(NoopEventSink), 60);
        notifier.tick().unwrap();

        let status: String = db
            .with_conn(|conn| {
                conn.query_row("SELECT status FROM space_events WHERE id = 'e3'", [], |r| {
                    r.get(0)
                })
                .map_err(anyhow::Error::from)
            })
            .unwrap();
        assert_eq!(status, "done");
    }

    #[test]
    fn start_notification_fires_for_event_without_reminder() {
        // The core guarantee: an event with NO reminder_min STILL pings at
        // its start time. This is the "Đi Uniqlo lúc 14h" case — created
        // without a reminder, must still notify when 14h arrives.
        let cfg = crate::config::Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let now_ms = chrono::Utc::now().timestamp_millis();
        db.with_conn(|conn| {
            // Started 1 min ago, ends in 59 min, NO reminder.
            insert_event(
                conn,
                "ev-start",
                "Đi Uniqlo",
                now_ms - 60_000,
                now_ms + 59 * 60_000,
                None,
                None,
            );
            Ok(())
        })
        .unwrap();

        let rec = Arc::new(Recorder(Mutex::new(vec![])));
        let notifier = EventNotifier::new(db.clone(), rec.clone(), 60);
        notifier.tick().unwrap();

        let fired = rec.0.lock().unwrap().clone();
        assert!(
            fired
                .iter()
                .any(|(_, id, kind, _)| id == "ev-start" && kind == "start"),
            "start notification must fire even without reminder_min; got {fired:?}"
        );
        // Persisted as a `start` notification row.
        let rows = db.list_event_notifications(None).unwrap();
        assert!(rows
            .iter()
            .any(|r| r.event_id == "ev-start" && r.kind == "start"));
    }

    #[test]
    fn start_notification_fires_exactly_once() {
        let cfg = crate::config::Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let now_ms = chrono::Utc::now().timestamp_millis();
        db.with_conn(|conn| {
            insert_event(
                conn,
                "ev-once",
                "Họp",
                now_ms - 30_000,
                now_ms + 30 * 60_000,
                None,
                None,
            );
            Ok(())
        })
        .unwrap();

        let rec = Arc::new(Recorder(Mutex::new(vec![])));
        let notifier = EventNotifier::new(db, rec.clone(), 60);
        notifier.tick().unwrap();
        notifier.tick().unwrap();
        notifier.tick().unwrap();

        let count = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, id, kind, _)| id == "ev-once" && kind == "start")
            .count();
        assert_eq!(count, 1, "start notification must be exactly-once");
    }

    #[test]
    fn future_event_does_not_fire_start_yet() {
        let cfg = crate::config::Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let now_ms = chrono::Utc::now().timestamp_millis();
        db.with_conn(|conn| {
            // Starts in 10 min — start notification must NOT fire now.
            insert_event(
                conn,
                "ev-future",
                "Tương lai",
                now_ms + 10 * 60_000,
                now_ms + 70 * 60_000,
                None,
                None,
            );
            Ok(())
        })
        .unwrap();

        let rec = Arc::new(Recorder(Mutex::new(vec![])));
        let notifier = EventNotifier::new(db, rec.clone(), 60);
        notifier.tick().unwrap();

        let fired = rec.0.lock().unwrap().clone();
        assert!(
            !fired.iter().any(|(_, id, _, _)| id == "ev-future"),
            "future event must not fire start notification yet"
        );
    }
}

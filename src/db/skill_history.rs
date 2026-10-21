//! Persistence for skill revisions (`skill_revisions`).
//!
//! One row per change to a skill directory. The content itself is on disk
//! (`skills::history`); the row keeps what is needed to list, compare and
//! decide: the hash, the diff against the previous revision, the scan result,
//! and where the revision came from. Rows are never updated except to mark a
//! snapshot as pruned.

use anyhow::Result;
use rusqlite::{params, OptionalExtension, Row};
use serde::Serialize;

/// Origins a revision can carry.
pub mod origin {
    /// Written through the daemon's API (create, overwrite, readme save).
    pub const MANUAL: &str = "MANUAL";
    /// Installed from the skill hub.
    pub const IMPORTED: &str = "IMPORTED";
    /// Restored from an earlier revision.
    pub const ROLLBACK: &str = "ROLLBACK";
    /// The skill directory is gone. Carries no snapshot of its own.
    pub const REMOVED: &str = "REMOVED";
    /// Found changed on disk without the daemon having written it (CLI, kit,
    /// a hand edit) — or captured just before the daemon overwrote it.
    pub const DETECTED: &str = "DETECTED";
    /// First time the skill was seen; nothing earlier is known.
    pub const BASELINE: &str = "BASELINE";
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRevision {
    pub id: String,
    pub skill_key: String,
    pub skill_name: String,
    pub source: String,
    pub parent_id: Option<String>,
    pub generation: i64,
    pub origin: String,
    pub actor: String,
    pub content_hash: String,
    pub file_count: i64,
    pub bytes: i64,
    pub truncated: bool,
    pub has_snapshot: bool,
    pub restored_from: Option<String>,
    pub change_summary: String,
    pub shadows: Option<String>,
    pub scan: Option<serde_json::Value>,
    pub created_at: i64,
    /// Unified diff against the parent. Only filled by [`Db::get_skill_revision`];
    /// listings leave it out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// What a caller supplies; the id, generation, parent and time are the store's.
pub struct NewSkillRevision<'a> {
    pub id: &'a str,
    pub skill_key: &'a str,
    pub skill_name: &'a str,
    pub source: &'a str,
    pub origin: &'a str,
    pub actor: &'a str,
    pub content_hash: &'a str,
    pub file_count: i64,
    pub bytes: i64,
    pub truncated: bool,
    pub has_snapshot: bool,
    pub restored_from: Option<&'a str>,
    pub change_summary: &'a str,
    pub diff: &'a str,
    pub shadows: Option<&'a str>,
    pub scan: Option<&'a serde_json::Value>,
}

const COLUMNS: &str = "id, skill_key, skill_name, source, parent_id, generation, origin, actor, \
     content_hash, file_count, bytes, truncated, has_snapshot, restored_from, change_summary, \
     shadows, scan, created_at";

fn row_to_revision(r: &Row<'_>) -> rusqlite::Result<SkillRevision> {
    let scan: Option<String> = r.get(16)?;
    Ok(SkillRevision {
        id: r.get(0)?,
        skill_key: r.get(1)?,
        skill_name: r.get(2)?,
        source: r.get(3)?,
        parent_id: r.get(4)?,
        generation: r.get(5)?,
        origin: r.get(6)?,
        actor: r.get(7)?,
        content_hash: r.get(8)?,
        file_count: r.get(9)?,
        bytes: r.get(10)?,
        truncated: r.get::<_, i64>(11)? != 0,
        has_snapshot: r.get::<_, i64>(12)? != 0,
        restored_from: r.get(13)?,
        change_summary: r.get(14)?,
        shadows: r.get(15)?,
        scan: scan.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: r.get(17)?,
        diff: None,
    })
}

impl super::Db {
    /// Append a revision. Its parent is the newest earlier revision of the same
    /// `(source, skill_key)` and its generation is the parent's plus one.
    pub fn insert_skill_revision(&self, n: &NewSkillRevision<'_>) -> Result<SkillRevision> {
        let now = chrono::Utc::now().timestamp_millis();
        let scan = n.scan.map(|v| v.to_string());
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let parent: Option<(String, i64)> = tx
                .query_row(
                    "SELECT id, generation FROM skill_revisions \
                     WHERE source = ?1 AND skill_key = ?2 \
                     ORDER BY created_at DESC, rowid DESC LIMIT 1",
                    params![n.source, n.skill_key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (parent_id, generation) = match parent {
                Some((id, g)) => (Some(id), g + 1),
                None => (None, 0),
            };
            tx.execute(
                "INSERT INTO skill_revisions \
                   (id, skill_key, skill_name, source, parent_id, generation, origin, actor, \
                    content_hash, file_count, bytes, truncated, has_snapshot, restored_from, \
                    change_summary, diff_text, shadows, scan, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                params![
                    n.id,
                    n.skill_key,
                    n.skill_name,
                    n.source,
                    parent_id,
                    generation,
                    n.origin,
                    n.actor,
                    n.content_hash,
                    n.file_count,
                    n.bytes,
                    n.truncated as i64,
                    n.has_snapshot as i64,
                    n.restored_from,
                    n.change_summary,
                    n.diff,
                    n.shadows,
                    scan,
                    now
                ],
            )?;
            tx.commit()?;
            Ok(SkillRevision {
                id: n.id.to_string(),
                skill_key: n.skill_key.to_string(),
                skill_name: n.skill_name.to_string(),
                source: n.source.to_string(),
                parent_id,
                generation,
                origin: n.origin.to_string(),
                actor: n.actor.to_string(),
                content_hash: n.content_hash.to_string(),
                file_count: n.file_count,
                bytes: n.bytes,
                truncated: n.truncated,
                has_snapshot: n.has_snapshot,
                restored_from: n.restored_from.map(str::to_string),
                change_summary: n.change_summary.to_string(),
                shadows: n.shadows.map(str::to_string),
                scan: n.scan.cloned(),
                created_at: now,
                diff: None,
            })
        })
    }

    /// The newest revision of a skill, whatever its origin.
    pub fn latest_skill_revision(
        &self,
        source: &str,
        skill_key: &str,
    ) -> Result<Option<SkillRevision>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM skill_revisions WHERE source = ?1 AND skill_key = ?2 \
                     ORDER BY created_at DESC, rowid DESC LIMIT 1"
                ),
                params![source, skill_key],
                row_to_revision,
            )
            .optional()?)
        })
    }

    /// Newest first, without the diffs.
    pub fn list_skill_revisions(
        &self,
        source: &str,
        skill_key: &str,
        limit: usize,
    ) -> Result<Vec<SkillRevision>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {COLUMNS} FROM skill_revisions WHERE source = ?1 AND skill_key = ?2 \
                 ORDER BY created_at DESC, rowid DESC LIMIT ?3"
            ))?;
            let rows = stmt
                .query_map(params![source, skill_key, limit as i64], row_to_revision)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// One revision with its diff.
    pub fn get_skill_revision(&self, id: &str) -> Result<Option<SkillRevision>> {
        self.with_conn(|c| {
            let row = c
                .query_row(
                    &format!("SELECT {COLUMNS}, diff_text FROM skill_revisions WHERE id = ?1"),
                    params![id],
                    |r| {
                        let mut rev = row_to_revision(r)?;
                        rev.diff = Some(r.get(18)?);
                        Ok(rev)
                    },
                )
                .optional()?;
            Ok(row)
        })
    }

    /// Skill keys of a source whose newest revision is not a removal: the ones
    /// the reconcile pass expects to find on disk.
    pub fn live_skill_keys(&self, source: &str) -> Result<Vec<String>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT skill_key FROM skill_revisions r WHERE source = ?1 \
                   AND origin <> 'REMOVED' \
                   AND NOT EXISTS ( \
                     SELECT 1 FROM skill_revisions n WHERE n.source = r.source \
                       AND n.skill_key = r.skill_key \
                       AND (n.created_at > r.created_at OR (n.created_at = r.created_at AND n.rowid > r.rowid)) \
                   )",
            )?;
            let keys = stmt
                .query_map(params![source], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(keys)
        })
    }

    /// Snapshot-bearing revisions beyond the newest `keep`, oldest last.
    pub fn snapshots_beyond(
        &self,
        source: &str,
        skill_key: &str,
        keep: usize,
    ) -> Result<Vec<String>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM skill_revisions WHERE source = ?1 AND skill_key = ?2 AND has_snapshot = 1 \
                 ORDER BY created_at DESC, rowid DESC LIMIT -1 OFFSET ?3",
            )?;
            let ids = stmt
                .query_map(params![source, skill_key, keep as i64], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(ids)
        })
    }

    pub fn mark_skill_snapshot_pruned(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE skill_revisions SET has_snapshot = 0 WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> super::super::Db {
        let config = crate::config::Config::from_env();
        super::super::Db::open_in_memory(&config).unwrap()
    }

    fn new<'a>(id: &'a str, key: &'a str, origin: &'a str, hash: &'a str) -> NewSkillRevision<'a> {
        NewSkillRevision {
            id,
            skill_key: key,
            skill_name: key,
            source: "clawhub-managed",
            origin,
            actor: "test",
            content_hash: hash,
            file_count: 1,
            bytes: 10,
            truncated: false,
            has_snapshot: origin != origin::REMOVED,
            restored_from: None,
            change_summary: "s",
            diff: "d",
            shadows: None,
            scan: None,
        }
    }

    #[test]
    fn revisions_chain_by_parent_and_generation() {
        let db = db();
        let a = db
            .insert_skill_revision(&new("r1", "weather", origin::MANUAL, "h1"))
            .unwrap();
        let b = db
            .insert_skill_revision(&new("r2", "weather", origin::MANUAL, "h2"))
            .unwrap();
        let other = db
            .insert_skill_revision(&new("r3", "dates", origin::MANUAL, "h3"))
            .unwrap();
        assert_eq!((a.parent_id.as_deref(), a.generation), (None, 0));
        assert_eq!((b.parent_id.as_deref(), b.generation), (Some("r1"), 1));
        assert_eq!((other.parent_id.as_deref(), other.generation), (None, 0));
        let latest = db
            .latest_skill_revision("clawhub-managed", "weather")
            .unwrap()
            .unwrap();
        assert_eq!(latest.id, "r2");
    }

    #[test]
    fn listing_leaves_the_diff_out_and_get_brings_it_back() {
        let db = db();
        db.insert_skill_revision(&new("r1", "weather", origin::MANUAL, "h1"))
            .unwrap();
        let listed = db
            .list_skill_revisions("clawhub-managed", "weather", 10)
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].diff.is_none());
        let full = db.get_skill_revision("r1").unwrap().unwrap();
        assert_eq!(full.diff.as_deref(), Some("d"));
        assert!(db.get_skill_revision("nope").unwrap().is_none());
    }

    #[test]
    fn scan_round_trips_as_json() {
        let db = db();
        let scan = serde_json::json!({"verdict": "warn", "findings": [{"rule": "INJ001"}]});
        let mut n = new("r1", "weather", origin::MANUAL, "h1");
        n.scan = Some(&scan);
        db.insert_skill_revision(&n).unwrap();
        let got = db
            .latest_skill_revision("clawhub-managed", "weather")
            .unwrap()
            .unwrap();
        assert_eq!(got.scan.unwrap()["findings"][0]["rule"], "INJ001");
    }

    #[test]
    fn live_keys_exclude_removed_and_superseded_rows() {
        let db = db();
        db.insert_skill_revision(&new("r1", "weather", origin::MANUAL, "h1"))
            .unwrap();
        db.insert_skill_revision(&new("r2", "weather", origin::MANUAL, "h2"))
            .unwrap();
        db.insert_skill_revision(&new("r3", "gone", origin::MANUAL, "h3"))
            .unwrap();
        db.insert_skill_revision(&new("r4", "gone", origin::REMOVED, "h3"))
            .unwrap();
        db.insert_skill_revision(&new("r5", "back", origin::MANUAL, "h5"))
            .unwrap();
        db.insert_skill_revision(&new("r6", "back", origin::REMOVED, "h5"))
            .unwrap();
        db.insert_skill_revision(&new("r7", "back", origin::MANUAL, "h7"))
            .unwrap();
        let mut keys = db.live_skill_keys("clawhub-managed").unwrap();
        keys.sort();
        assert_eq!(keys, vec!["back".to_string(), "weather".to_string()]);
    }

    #[test]
    fn snapshots_beyond_the_newest_are_listed_for_pruning() {
        let db = db();
        for i in 0..5 {
            db.insert_skill_revision(&new(
                &format!("r{i}"),
                "weather",
                origin::MANUAL,
                &format!("h{i}"),
            ))
            .unwrap();
        }
        let old = db
            .snapshots_beyond("clawhub-managed", "weather", 2)
            .unwrap();
        assert_eq!(
            old,
            vec!["r2".to_string(), "r1".to_string(), "r0".to_string()]
        );
        db.mark_skill_snapshot_pruned("r0").unwrap();
        assert!(!db.get_skill_revision("r0").unwrap().unwrap().has_snapshot);
        let old = db
            .snapshots_beyond("clawhub-managed", "weather", 2)
            .unwrap();
        assert_eq!(old, vec!["r2".to_string(), "r1".to_string()]);
    }

    #[test]
    fn two_sources_keep_separate_chains() {
        let db = db();
        db.insert_skill_revision(&new("r1", "web-research", origin::MANUAL, "h1"))
            .unwrap();
        let mut b = new("r2", "web-research", origin::BASELINE, "h2");
        b.source = "bundled";
        let r2 = db.insert_skill_revision(&b).unwrap();
        assert_eq!((r2.parent_id, r2.generation), (None, 0));
    }
}

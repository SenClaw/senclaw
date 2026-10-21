//! Persistence for the skill usage ledger (`skill_usage`).
//!
//! One row per time a skill's instructions were loaded into a chat, closed by
//! what the turn did next. No prompt, no tool argument and no model output is
//! kept — the row says which skill (by directory, name and content hash), how
//! it was loaded, and how the turn ended.

use anyhow::Result;
use rusqlite::{params, Row};
use serde::Serialize;

/// Rows kept; the oldest go first.
const MAX_ROWS: i64 = 50_000;

/// How a turn ended, as far as the event bus could tell.
pub mod outcome {
    pub const OPEN: &str = "open";
    /// The main agent gave a final answer.
    pub const COMPLETED: &str = "completed";
    /// The session reported an error (a tool-error loop included).
    pub const ERRORED: &str = "errored";
    pub const INTERRUPTED: &str = "interrupted";
    /// The daemon stopped, or the chat never reported an end.
    pub const UNKNOWN: &str = "unknown";
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillUsageRow {
    pub id: i64,
    pub at: i64,
    pub chat_jid: String,
    pub skill_name: String,
    pub skill_key: String,
    pub source: String,
    pub content_hash: Option<String>,
    /// `tool` (the model called `Skill`) or `forced` (`#name`, `/name`,
    /// pre-trigger, router).
    pub how: String,
    pub outcome: String,
    pub ended_at: Option<i64>,
    pub tool_failures: i64,
    pub first_failed_tool: Option<String>,
    pub loop_stopped: bool,
}

/// One skill at one content hash, counted.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageGroup {
    pub source: String,
    pub skill_key: String,
    pub skill_name: String,
    pub content_hash: Option<String>,
    pub loads: i64,
    pub completed: i64,
    pub errored: i64,
    pub interrupted: i64,
    /// Still open or never closed.
    pub unfinished: i64,
    pub with_tool_failures: i64,
    pub loop_stopped: i64,
    pub by_tool: i64,
    pub forced: i64,
    pub first_at: i64,
    pub last_at: i64,
}

const COLUMNS: &str =
    "id, at, chat_jid, skill_name, skill_key, source, content_hash, how, outcome, \
     ended_at, tool_failures, first_failed_tool, loop_stopped";

fn row(r: &Row<'_>) -> rusqlite::Result<SkillUsageRow> {
    Ok(SkillUsageRow {
        id: r.get(0)?,
        at: r.get(1)?,
        chat_jid: r.get(2)?,
        skill_name: r.get(3)?,
        skill_key: r.get(4)?,
        source: r.get(5)?,
        content_hash: r.get(6)?,
        how: r.get(7)?,
        outcome: r.get(8)?,
        ended_at: r.get(9)?,
        tool_failures: r.get(10)?,
        first_failed_tool: r.get(11)?,
        loop_stopped: r.get::<_, i64>(12)? != 0,
    })
}

impl super::Db {
    #[allow(clippy::too_many_arguments)]
    pub fn insert_skill_usage(
        &self,
        at: i64,
        chat_jid: &str,
        skill_name: &str,
        skill_key: &str,
        source: &str,
        content_hash: Option<&str>,
        how: &str,
    ) -> Result<i64> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO skill_usage (at, chat_jid, skill_name, skill_key, source, content_hash, how) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![at, chat_jid, skill_name, skill_key, source, content_hash, how],
            )?;
            let id = c.last_insert_rowid();
            c.execute(
                "DELETE FROM skill_usage WHERE id <= (SELECT MAX(id) FROM skill_usage) - ?1",
                params![MAX_ROWS],
            )?;
            Ok(id)
        })
    }

    /// A tool failed during a turn that has skills loaded: count it on each.
    pub fn skill_usage_tool_failed(&self, chat_jid: &str, tool: &str) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "UPDATE skill_usage SET tool_failures = tool_failures + 1, \
                   first_failed_tool = COALESCE(first_failed_tool, ?2) \
                 WHERE chat_jid = ?1 AND outcome = 'open'",
                params![chat_jid, tool],
            )?)
        })
    }

    /// Close the chat's open rows with `outcome`; returns how many closed.
    pub fn close_skill_usage(
        &self,
        chat_jid: &str,
        outcome: &str,
        loop_stopped: bool,
        at: i64,
    ) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "UPDATE skill_usage SET outcome = ?2, ended_at = ?3, loop_stopped = ?4 \
                 WHERE chat_jid = ?1 AND outcome = 'open'",
                params![chat_jid, outcome, at, loop_stopped as i64],
            )?)
        })
    }

    /// Rows a stopped daemon left open will never be closed.
    pub fn close_stale_skill_usage(&self, at: i64) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "UPDATE skill_usage SET outcome = 'unknown', ended_at = ?1 WHERE outcome = 'open'",
                params![at],
            )?)
        })
    }

    /// Chats that have an open row, for seeding the in-memory filter.
    pub fn chats_with_open_skill_usage(&self) -> Result<Vec<String>> {
        self.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT DISTINCT chat_jid FROM skill_usage WHERE outcome = 'open'")?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Newest first; `skill` matches the directory key or the name.
    pub fn list_skill_usage(
        &self,
        skill: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SkillUsageRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {COLUMNS} FROM skill_usage \
                 WHERE (?1 IS NULL OR skill_key = ?1 OR skill_name = ?1) \
                 ORDER BY id DESC LIMIT ?2"
            ))?;
            let rows = stmt
                .query_map(params![skill, limit as i64], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Counts per `(source, skill, content hash)` for loads since `since_ms`.
    pub fn skill_usage_groups(&self, since_ms: i64) -> Result<Vec<UsageGroup>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT source, skill_key, MAX(skill_name), content_hash, COUNT(*), \
                   SUM(outcome = 'completed'), SUM(outcome = 'errored'), SUM(outcome = 'interrupted'), \
                   SUM(outcome IN ('open', 'unknown')), \
                   SUM(tool_failures > 0), SUM(loop_stopped), SUM(how = 'tool'), SUM(how = 'forced'), \
                   MIN(at), MAX(at) \
                 FROM skill_usage WHERE at >= ?1 \
                 GROUP BY source, skill_key, content_hash \
                 ORDER BY source, skill_key, MIN(at)",
            )?;
            let rows = stmt
                .query_map(params![since_ms], |r| {
                    Ok(UsageGroup {
                        source: r.get(0)?,
                        skill_key: r.get(1)?,
                        skill_name: r.get(2)?,
                        content_hash: r.get(3)?,
                        loads: r.get(4)?,
                        completed: r.get(5)?,
                        errored: r.get(6)?,
                        interrupted: r.get(7)?,
                        unfinished: r.get(8)?,
                        with_tool_failures: r.get(9)?,
                        loop_stopped: r.get(10)?,
                        by_tool: r.get(11)?,
                        forced: r.get(12)?,
                        first_at: r.get(13)?,
                        last_at: r.get(14)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The newest revision row whose content is this hash, for naming the
    /// version a usage group belongs to.
    pub fn revision_for_hash(
        &self,
        source: &str,
        skill_key: &str,
        hash: &str,
    ) -> Result<Option<(String, String, i64)>> {
        use rusqlite::OptionalExtension;
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT id, origin, generation FROM skill_revisions \
                 WHERE source = ?1 AND skill_key = ?2 AND content_hash = ?3 AND origin <> 'REMOVED' \
                 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                params![source, skill_key, hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> super::super::Db {
        super::super::Db::open_in_memory(&crate::config::Config::from_env()).unwrap()
    }

    fn load(
        db: &super::super::Db,
        at: i64,
        jid: &str,
        key: &str,
        hash: Option<&str>,
        how: &str,
    ) -> i64 {
        db.insert_skill_usage(at, jid, key, key, "clawhub-managed", hash, how)
            .unwrap()
    }

    #[test]
    fn a_turn_end_closes_every_open_row_of_that_chat_only() {
        let db = db();
        load(&db, 1, "web:a", "wiki", Some("h1"), "tool");
        load(&db, 2, "web:a", "code", Some("h2"), "forced");
        load(&db, 3, "web:b", "wiki", Some("h1"), "tool");
        assert_eq!(
            db.close_skill_usage("web:a", outcome::COMPLETED, false, 10)
                .unwrap(),
            2
        );
        let rows = db.list_skill_usage(None, 10).unwrap();
        let by_chat = |jid: &str| {
            rows.iter()
                .filter(|r| r.chat_jid == jid)
                .map(|r| r.outcome.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(by_chat("web:a"), vec!["completed", "completed"]);
        assert_eq!(by_chat("web:b"), vec!["open"]);
        assert_eq!(
            db.close_skill_usage("web:a", outcome::ERRORED, false, 11)
                .unwrap(),
            0,
            "closed rows stay closed"
        );
    }

    #[test]
    fn a_failing_tool_is_counted_on_open_rows_and_remembers_the_first() {
        let db = db();
        load(&db, 1, "web:a", "wiki", None, "tool");
        db.skill_usage_tool_failed("web:a", "Bash").unwrap();
        db.skill_usage_tool_failed("web:a", "Read").unwrap();
        db.close_skill_usage("web:a", outcome::COMPLETED, false, 5)
            .unwrap();
        assert_eq!(
            db.skill_usage_tool_failed("web:a", "Bash").unwrap(),
            0,
            "nothing open any more"
        );
        let r = &db.list_skill_usage(None, 1).unwrap()[0];
        assert_eq!(
            (r.tool_failures, r.first_failed_tool.as_deref()),
            (2, Some("Bash"))
        );
    }

    #[test]
    fn stale_open_rows_become_unknown() {
        let db = db();
        load(&db, 1, "web:a", "wiki", None, "tool");
        assert_eq!(
            db.chats_with_open_skill_usage().unwrap(),
            vec!["web:a".to_string()]
        );
        assert_eq!(db.close_stale_skill_usage(9).unwrap(), 1);
        assert!(db.chats_with_open_skill_usage().unwrap().is_empty());
        assert_eq!(
            db.list_skill_usage(None, 1).unwrap()[0].outcome,
            outcome::UNKNOWN
        );
    }

    #[test]
    fn groups_count_by_skill_and_content_hash() {
        let db = db();
        for (i, (hash, out)) in [("h1", "completed"), ("h1", "errored"), ("h2", "completed")]
            .iter()
            .enumerate()
        {
            load(&db, 10 + i as i64, "web:a", "wiki", Some(*hash), "tool");
            db.skill_usage_tool_failed("web:a", "Bash").unwrap();
            db.close_skill_usage("web:a", out, *out == "errored", 20)
                .unwrap();
        }
        load(&db, 30, "web:a", "wiki", Some("h2"), "forced"); // still open
        let g = db.skill_usage_groups(0).unwrap();
        assert_eq!(g.len(), 2);
        let h1 = g
            .iter()
            .find(|x| x.content_hash.as_deref() == Some("h1"))
            .unwrap();
        assert_eq!(
            (
                h1.loads,
                h1.completed,
                h1.errored,
                h1.with_tool_failures,
                h1.loop_stopped
            ),
            (2, 1, 1, 2, 1)
        );
        let h2 = g
            .iter()
            .find(|x| x.content_hash.as_deref() == Some("h2"))
            .unwrap();
        assert_eq!(
            (h2.loads, h2.completed, h2.unfinished, h2.by_tool, h2.forced),
            (2, 1, 1, 1, 1)
        );
        assert_eq!(
            db.skill_usage_groups(25).unwrap().len(),
            1,
            "the window cuts older loads"
        );
    }

    #[test]
    fn listing_filters_by_key_or_name_and_the_oldest_rows_are_trimmed() {
        let db = db();
        db.insert_skill_usage(
            1,
            "web:a",
            "Web Research",
            "web-research",
            "bundled",
            None,
            "tool",
        )
        .unwrap();
        load(&db, 2, "web:a", "wiki", None, "tool");
        assert_eq!(
            db.list_skill_usage(Some("web-research"), 10).unwrap().len(),
            1
        );
        assert_eq!(
            db.list_skill_usage(Some("Web Research"), 10).unwrap().len(),
            1
        );
        assert_eq!(db.list_skill_usage(None, 10).unwrap().len(), 2);
        assert_eq!(
            db.list_skill_usage(None, 1).unwrap()[0].skill_key,
            "wiki",
            "newest first"
        );
    }

    #[test]
    fn a_hash_is_named_by_its_newest_revision() {
        use crate::db::skill_history::{origin, NewSkillRevision};
        let db = db();
        let rev = |id: &'static str, hash: &'static str| NewSkillRevision {
            id,
            skill_key: "wiki",
            skill_name: "wiki",
            source: "clawhub-managed",
            origin: origin::MANUAL,
            actor: "t",
            content_hash: hash,
            file_count: 1,
            bytes: 1,
            truncated: false,
            has_snapshot: true,
            restored_from: None,
            change_summary: "",
            diff: "",
            shadows: None,
            scan: None,
        };
        db.insert_skill_revision(&rev("r1", "h1")).unwrap();
        db.insert_skill_revision(&rev("r2", "h2")).unwrap();
        db.insert_skill_revision(&rev("r3", "h1")).unwrap();
        let (id, _, generation) = db
            .revision_for_hash("clawhub-managed", "wiki", "h1")
            .unwrap()
            .unwrap();
        assert_eq!((id.as_str(), generation), ("r3", 2));
        assert!(db
            .revision_for_hash("clawhub-managed", "wiki", "nope")
            .unwrap()
            .is_none());
    }
}

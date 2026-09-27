//! Persistence for the failure ledger (`failure_episodes`).
//!
//! One row per *episode*: a run of consecutive failures of the same tool in
//! the same chat, plus the outcome that followed (did the tool ever succeed,
//! and was it the model or the user who supplied the fix). Written by
//! [`crate::failures`]; read by `/api/failures*`.
//!
//! Nothing here feeds a prompt. The ledger exists to answer three questions
//! before any self-improvement machinery is built: how often does the same
//! failure come back, how often does it get fixed at all, and who fixes it.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

/// An episode row as the API returns it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureEpisodeRow {
    pub id: i64,
    pub chat_jid: String,
    pub agent_id: String,
    pub tool_name: String,
    /// Argument name -> JSON type of the failing call. Never values.
    pub args_shape: serde_json::Value,
    pub first_error: String,
    pub last_error: String,
    pub error_class: String,
    pub streak: i64,
    pub status: String,
    pub fix_source: Option<String>,
    pub loop_stopped: bool,
    pub user_input_after: bool,
    pub turns_ended: i64,
    pub first_error_at: String,
    pub last_error_at: String,
    pub closed_at: Option<String>,
}

/// Rows kept. Errors are rare enough that this is months of history, but the
/// table must still be bounded — it is never trimmed with the chat.
const MAX_ROWS: i64 = 20_000;

/// How long an open episode stays extendable. Past this the next failure of
/// the same tool is a *new* episode: a daemon that restarted mid-episode, or a
/// chat picked up the next morning, must not glue a week's failures into one
/// run with a meaningless streak.
const EPISODE_GAP_SECS: i64 = 6 * 3600;

/// Turns an episode may survive before it counts as given up. One is the turn
/// that failed; the second is the turn where a user correction would land.
/// After that, crediting a success to this failure is guesswork.
const MAX_TURNS_OPEN: i64 = 2;

impl super::Db {
    /// Record one tool failure: extend the chat's open episode for that tool,
    /// or start a new one. Returns the episode id.
    #[allow(clippy::too_many_arguments)]
    pub fn record_tool_failure(
        &self,
        chat_jid: &str,
        agent_id: &str,
        tool_name: &str,
        args_shape: &str,
        error_text: &str,
        error_class: &str,
        at: &str,
    ) -> Result<i64> {
        self.with_conn(|c| {
            let open: Option<(i64, String)> = c
                .query_row(
                    "SELECT id, last_error_at FROM failure_episodes \
                     WHERE chat_jid = ?1 AND tool_name = ?2 AND status = 'open' \
                     ORDER BY id DESC LIMIT 1",
                    params![chat_jid, tool_name],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;

            if let Some((id, last_at)) = open {
                if within_gap(&last_at, at) {
                    c.execute(
                        "UPDATE failure_episodes \
                         SET streak = streak + 1, last_error = ?2, last_error_at = ?3, \
                             args_shape = ?4 \
                         WHERE id = ?1",
                        params![id, error_text, at, args_shape],
                    )?;
                    return Ok(id);
                }
                // Stale: close it before opening a fresh episode, so the old
                // row does not sit 'open' forever and swallow later failures.
                c.execute(
                    "UPDATE failure_episodes SET status = 'gave_up', closed_at = ?2 \
                     WHERE id = ?1 AND status = 'open'",
                    params![id, at],
                )?;
            }

            c.execute(
                "INSERT INTO failure_episodes \
                   (chat_jid, agent_id, tool_name, args_shape, first_error, last_error, \
                    error_class, streak, status, first_error_at, last_error_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, 1, 'open', ?7, ?7)",
                params![
                    chat_jid,
                    agent_id,
                    tool_name,
                    args_shape,
                    error_text,
                    error_class,
                    at
                ],
            )?;
            let id = c.last_insert_rowid();
            c.execute(
                "DELETE FROM failure_episodes WHERE id <= \
                   (SELECT MAX(id) FROM failure_episodes) - ?1",
                params![MAX_ROWS],
            )?;
            Ok(id)
        })
    }

    /// The tool succeeded: close the chat's open episode for it. `fix_source`
    /// is `user` when a user message arrived after the first failure, else
    /// `model`. Returns how many episodes closed.
    pub fn resolve_tool_failure(&self, chat_jid: &str, tool_name: &str, at: &str) -> Result<usize> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE failure_episodes \
                 SET status = 'resolved', closed_at = ?3, \
                     fix_source = CASE WHEN user_input_after = 1 THEN 'user' ELSE 'model' END \
                 WHERE chat_jid = ?1 AND tool_name = ?2 AND status = 'open'",
                params![chat_jid, tool_name, at],
            )?;
            Ok(n)
        })
    }

    /// A user message arrived: any failure still open in this chat may be
    /// fixed by what the user just said, not by the model's own next attempt.
    pub fn mark_failures_user_input(&self, chat_jid: &str) -> Result<usize> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE failure_episodes SET user_input_after = 1 \
                 WHERE chat_jid = ?1 AND status = 'open'",
                params![chat_jid],
            )?;
            Ok(n)
        })
    }

    /// An agent turn ended with a final answer. Episodes that have now lived
    /// through [`MAX_TURNS_OPEN`] turns without the tool ever succeeding are
    /// given up. Returns how many were closed.
    pub fn end_turn_for_failures(&self, chat_jid: &str, at: &str) -> Result<usize> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE failure_episodes SET turns_ended = turns_ended + 1 \
                 WHERE chat_jid = ?1 AND status = 'open'",
                params![chat_jid],
            )?;
            let n = c.execute(
                "UPDATE failure_episodes SET status = 'gave_up', closed_at = ?2 \
                 WHERE chat_jid = ?1 AND status = 'open' AND turns_ended >= ?3",
                params![chat_jid, at, MAX_TURNS_OPEN],
            )?;
            Ok(n)
        })
    }

    /// The engine's error-loop guard hard-stopped on `tool_name`.
    pub fn mark_failure_loop_stopped(&self, chat_jid: &str, tool_name: &str) -> Result<usize> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE failure_episodes SET loop_stopped = 1 \
                 WHERE chat_jid = ?1 AND tool_name = ?2 AND status = 'open'",
                params![chat_jid, tool_name],
            )?;
            Ok(n)
        })
    }

    /// Recent episodes, newest first. `chat_jid` / `status` narrow the list.
    pub fn list_failure_episodes(
        &self,
        chat_jid: Option<&str>,
        status: Option<&str>,
        limit: u32,
    ) -> Result<Vec<FailureEpisodeRow>> {
        self.with_conn(|c| {
            let mut sql = String::from(
                "SELECT id, chat_jid, agent_id, tool_name, args_shape, first_error, last_error, \
                        error_class, streak, status, fix_source, loop_stopped, user_input_after, \
                        turns_ended, first_error_at, last_error_at, closed_at \
                 FROM failure_episodes WHERE 1 = 1",
            );
            let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(j) = chat_jid {
                sql.push_str(" AND chat_jid = ?");
                args.push(Box::new(j.to_string()));
            }
            if let Some(s) = status {
                sql.push_str(" AND status = ?");
                args.push(Box::new(s.to_string()));
            }
            sql.push_str(" ORDER BY id DESC LIMIT ?");
            args.push(Box::new(limit as i64));

            let mut stmt = c.prepare(&sql)?;
            let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(refs.as_slice(), |r| {
                let shape: String = r.get(4)?;
                Ok(FailureEpisodeRow {
                    id: r.get(0)?,
                    chat_jid: r.get(1)?,
                    agent_id: r.get(2)?,
                    tool_name: r.get(3)?,
                    args_shape: serde_json::from_str(&shape)
                        .unwrap_or_else(|_| serde_json::json!({})),
                    first_error: r.get(5)?,
                    last_error: r.get(6)?,
                    error_class: r.get(7)?,
                    streak: r.get(8)?,
                    status: r.get(9)?,
                    fix_source: r.get(10)?,
                    loop_stopped: r.get::<_, i64>(11)? != 0,
                    user_input_after: r.get::<_, i64>(12)? != 0,
                    turns_ended: r.get(13)?,
                    first_error_at: r.get(14)?,
                    last_error_at: r.get(15)?,
                    closed_at: r.get(16)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// The three numbers that decide whether learning from failures is worth
    /// building: how often a failure repeats, how often it gets fixed, and who
    /// fixed it. Plus the tools responsible, so a bad tool description can be
    /// fixed directly instead.
    pub fn failure_summary(&self, since: &str) -> Result<serde_json::Value> {
        self.with_conn(|c| {
            let total: i64 = c.query_row(
                "SELECT COUNT(*) FROM failure_episodes WHERE first_error_at >= ?1",
                params![since],
                |r| r.get(0),
            )?;

            // Repeat rate: episodes whose (tool, error class) pair occurs in
            // more than one episode. Below ~15% a lesson store would almost
            // never hit, and better tool descriptions are the cheaper fix.
            let repeated: i64 = c.query_row(
                "SELECT COALESCE(SUM(n), 0) FROM ( \
                   SELECT COUNT(*) AS n FROM failure_episodes \
                   WHERE first_error_at >= ?1 \
                   GROUP BY tool_name, error_class HAVING COUNT(*) > 1)",
                params![since],
                |r| r.get(0),
            )?;

            let mut by_status = serde_json::Map::new();
            {
                let mut stmt = c.prepare(
                    "SELECT status, COUNT(*) FROM failure_episodes \
                     WHERE first_error_at >= ?1 GROUP BY status",
                )?;
                let rows = stmt.query_map(params![since], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?;
                for row in rows {
                    let (k, v) = row?;
                    by_status.insert(k, serde_json::json!(v));
                }
            }

            let mut by_fix = serde_json::Map::new();
            {
                let mut stmt = c.prepare(
                    "SELECT fix_source, COUNT(*) FROM failure_episodes \
                     WHERE first_error_at >= ?1 AND status = 'resolved' \
                     GROUP BY fix_source",
                )?;
                let rows = stmt.query_map(params![since], |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        r.get::<_, i64>(1)?,
                    ))
                })?;
                for row in rows {
                    let (k, v) = row?;
                    by_fix.insert(k, serde_json::json!(v));
                }
            }

            // A model fix that took at least a nudge (streak >= 4) is the case
            // the existing nudge already handles; a user fix is the case only a
            // remembered lesson could have caught.
            let model_after_nudge: i64 = c.query_row(
                "SELECT COUNT(*) FROM failure_episodes \
                 WHERE first_error_at >= ?1 AND status = 'resolved' \
                   AND fix_source = 'model' AND streak >= 4",
                params![since],
                |r| r.get(0),
            )?;

            let mut tools = Vec::new();
            {
                let mut stmt = c.prepare(
                    "SELECT tool_name, error_class, COUNT(*) AS n, SUM(streak) \
                     FROM failure_episodes WHERE first_error_at >= ?1 \
                     GROUP BY tool_name, error_class ORDER BY n DESC LIMIT 20",
                )?;
                let rows = stmt.query_map(params![since], |r| {
                    Ok(serde_json::json!({
                        "tool": r.get::<_, String>(0)?,
                        "errorClass": r.get::<_, String>(1)?,
                        "episodes": r.get::<_, i64>(2)?,
                        "failedCalls": r.get::<_, i64>(3)?,
                    }))
                })?;
                for row in rows {
                    tools.push(row?);
                }
            }

            Ok(serde_json::json!({
                "since": since,
                "episodes": total,
                "repeatedEpisodes": repeated,
                "repeatRate": if total > 0 { repeated as f64 / total as f64 } else { 0.0 },
                "byStatus": by_status,
                "byFixSource": by_fix,
                "modelFixedAfterNudge": model_after_nudge,
                "topFailures": tools,
            }))
        })
    }
}

/// Is `now` still inside the open episode that last failed at `last`?
/// Unparsable timestamps count as inside: the alternative is splitting one
/// episode into singletons, which reads as "never repeats".
fn within_gap(last: &str, now: &str) -> bool {
    let (Ok(a), Ok(b)) = (
        chrono::DateTime::parse_from_rfc3339(last),
        chrono::DateTime::parse_from_rfc3339(now),
    ) else {
        return true;
    };
    (b - a).num_seconds().abs() <= EPISODE_GAP_SECS
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    use super::super::Db;

    fn db() -> Db {
        Db::open_in_memory(&Config::from_env()).unwrap()
    }

    fn fail(db: &Db, tool: &str, at: &str) -> i64 {
        db.record_tool_failure(
            "web:main",
            "main",
            tool,
            r#"{"host":"string:empty"}"#,
            "Missing host, port, or user",
            "missing host port or user",
            at,
        )
        .unwrap()
    }

    fn row(db: &Db, id: i64) -> super::FailureEpisodeRow {
        db.list_failure_episodes(None, None, 100)
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .expect("episode row")
    }

    #[test]
    fn consecutive_failures_extend_one_episode() {
        let db = db();
        let a = fail(&db, "ssh_start_connect", "2026-09-21T10:00:00Z");
        let b = fail(&db, "ssh_start_connect", "2026-09-21T10:00:30Z");
        assert_eq!(a, b, "same episode");
        let r = row(&db, a);
        assert_eq!(r.streak, 2);
        assert_eq!(r.status, "open");
        // Values never reach the ledger — only the shape of the call.
        assert_eq!(r.args_shape["host"], "string:empty");
    }

    #[test]
    fn a_success_with_no_user_message_credits_the_model() {
        let db = db();
        let id = fail(&db, "ssh_start_connect", "2026-09-21T10:00:00Z");
        assert_eq!(
            db.resolve_tool_failure("web:main", "ssh_start_connect", "2026-09-21T10:01:00Z")
                .unwrap(),
            1
        );
        let r = row(&db, id);
        assert_eq!(r.status, "resolved");
        assert_eq!(r.fix_source.as_deref(), Some("model"));
    }

    #[test]
    fn a_user_message_before_the_fix_credits_the_user() {
        let db = db();
        let id = fail(&db, "ssh_start_connect", "2026-09-21T10:00:00Z");
        db.mark_failures_user_input("web:main").unwrap();
        db.resolve_tool_failure("web:main", "ssh_start_connect", "2026-09-21T10:05:00Z")
            .unwrap();
        // This is the number the whole ledger is for: a fix only a remembered
        // lesson could have supplied, as opposed to one the existing nudge got.
        assert_eq!(row(&db, id).fix_source.as_deref(), Some("user"));
    }

    #[test]
    fn two_turns_without_a_fix_is_giving_up() {
        let db = db();
        let id = fail(&db, "space_app_start", "2026-09-21T10:00:00Z");
        // The turn that failed ends — still open, because a user correction
        // lands in the turn after it.
        db.end_turn_for_failures("web:main", "2026-09-21T10:01:00Z")
            .unwrap();
        assert_eq!(row(&db, id).status, "open");
        db.end_turn_for_failures("web:main", "2026-09-21T10:02:00Z")
            .unwrap();
        let r = row(&db, id);
        assert_eq!(r.status, "gave_up");
        assert_eq!(r.fix_source, None);
        assert_eq!(r.turns_ended, 2);
    }

    #[test]
    fn a_stale_episode_does_not_swallow_a_later_failure() {
        let db = db();
        let old = fail(&db, "wiki_write", "2026-09-21T10:00:00Z");
        // Next morning. Gluing these together would report one long streak
        // for a failure that actually happened twice — the opposite of what
        // the repeat rate is measuring.
        let new = fail(&db, "wiki_write", "2026-09-22T09:00:00Z");
        assert_ne!(old, new);
        assert_eq!(row(&db, old).status, "gave_up");
        assert_eq!(row(&db, new).streak, 1);
    }

    #[test]
    fn a_hard_stop_is_marked_on_the_open_episode() {
        let db = db();
        let id = fail(&db, "browser_click", "2026-09-21T10:00:00Z");
        assert_eq!(
            db.mark_failure_loop_stopped("web:main", "browser_click")
                .unwrap(),
            1
        );
        assert!(row(&db, id).loop_stopped);
    }

    #[test]
    fn the_summary_counts_repeats_by_tool_and_error_class() {
        let db = db();
        fail(&db, "ssh_start_connect", "2026-09-21T10:00:00Z");
        db.resolve_tool_failure("web:main", "ssh_start_connect", "2026-09-21T10:01:00Z")
            .unwrap();
        fail(&db, "ssh_start_connect", "2026-09-21T11:00:00Z");
        db.record_tool_failure(
            "web:main",
            "main",
            "wiki_write",
            "{}",
            "no such directory",
            "no such directory",
            "2026-09-21T12:00:00Z",
        )
        .unwrap();

        let s = db.failure_summary("2026-09-01T00:00:00Z").unwrap();
        assert_eq!(s["episodes"], 3);
        // The two ssh episodes share a class; the wiki one is a singleton.
        assert_eq!(s["repeatedEpisodes"], 2);
        assert_eq!(s["byStatus"]["resolved"], 1);
        assert_eq!(s["byStatus"]["open"], 2);
        assert_eq!(s["byFixSource"]["model"], 1);
        assert_eq!(s["topFailures"][0]["episodes"], 2);
    }

    #[test]
    fn filters_narrow_the_listing() {
        let db = db();
        fail(&db, "a_tool", "2026-09-21T10:00:00Z");
        db.record_tool_failure(
            "tg:group:9",
            "main",
            "b_tool",
            "{}",
            "boom",
            "boom",
            "2026-09-21T10:00:00Z",
        )
        .unwrap();
        assert_eq!(
            db.list_failure_episodes(Some("tg:group:9"), None, 50)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.list_failure_episodes(None, Some("gave_up"), 50)
                .unwrap()
                .len(),
            0
        );
    }
}

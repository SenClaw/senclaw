//! Persistence for `ToolExecutionEvent` rows so the chat UI can replay
//! tool-call cards after a page reload. Mirrors the trim-on-insert behaviour
//! used by [`Db::insert_group_message`] so a chatty agent can't grow this
//! table unboundedly per chat.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

/// One persisted tool execution. Maps 1:1 to the `tool:execution`
/// WebSocket frame and to `ToolMessage` on the client.
#[derive(Debug, Clone)]
pub struct StoredToolExecution {
    pub id: i64,
    pub chat_jid: String,
    pub agent_id: String,
    pub tool_name: String,
    pub title: String,
    pub summary: String,
    /// The model's own one-line account of what this step was for. Empty when
    /// the tool takes no `description`; the clients then show its verb.
    pub description: String,
    /// Serialised `content` (raw JSON text). The wire format passes this
    /// back through unchanged.
    pub content_json: String,
    pub ok: bool,
    pub timestamp: String,
}

impl super::Db {
    /// Insert a tool execution row and FIFO-trim the chat to its retention
    /// limit (same cap as `group_messages`, falling back to `default_limit`).
    pub fn insert_tool_execution(
        &self,
        chat_jid: &str,
        agent_id: &str,
        tool_name: &str,
        title: &str,
        summary: &str,
        description: &str,
        content_json: &str,
        ok: bool,
        timestamp: &str,
        default_limit: u32,
    ) -> Result<()> {
        self.with_conn(|c| {
            let limit: i64 = c
                .query_row(
                    "SELECT max_messages FROM groups WHERE jid = ?1",
                    params![chat_jid],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten()
                .unwrap_or(default_limit as i64);

            c.execute(
                r#"
                INSERT INTO tool_executions
                  (chat_jid, agent_id, tool_name, title, summary, description,
                   content_json, ok, timestamp)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                "#,
                params![
                    chat_jid,
                    agent_id,
                    tool_name,
                    title,
                    summary,
                    description,
                    content_json,
                    ok as i64,
                    timestamp,
                ],
            )?;

            c.execute(
                r#"
                DELETE FROM tool_executions
                WHERE chat_jid = ?1
                  AND id NOT IN (
                    SELECT id FROM tool_executions
                    WHERE chat_jid = ?1
                    ORDER BY timestamp DESC, id DESC
                    LIMIT ?2
                  )
                "#,
                params![chat_jid, limit],
            )?;
            Ok(())
        })
    }

    /// Fetch tool executions for a chat in chronological order (oldest first).
    /// `limit` caps the row count when set; `None` means "all rows".
    pub fn get_tool_executions(
        &self,
        chat_jid: &str,
        limit: Option<u32>,
    ) -> Result<Vec<StoredToolExecution>> {
        self.with_conn(|c| {
            let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<StoredToolExecution> {
                Ok(StoredToolExecution {
                    id: r.get(0)?,
                    chat_jid: r.get(1)?,
                    agent_id: r.get(2)?,
                    tool_name: r.get(3)?,
                    title: r.get(4)?,
                    summary: r.get(5)?,
                    description: r.get(6)?,
                    content_json: r.get(7)?,
                    ok: r.get::<_, i64>(8)? != 0,
                    timestamp: r.get(9)?,
                })
            };

            let rows: Vec<StoredToolExecution> = if let Some(lim) = limit {
                // Take the most recent `lim` rows then re-sort ascending so
                // the caller can merge with group_messages chronologically.
                let mut stmt = c.prepare(
                    "SELECT id, chat_jid, agent_id, tool_name, title, summary,
                            description, content_json, ok, timestamp
                     FROM tool_executions
                     WHERE chat_jid = ?1
                     ORDER BY timestamp DESC, id DESC
                     LIMIT ?2",
                )?;
                let mut v: Vec<StoredToolExecution> = stmt
                    .query_map(params![chat_jid, lim as i64], map_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                v.reverse();
                v
            } else {
                let mut stmt = c.prepare(
                    "SELECT id, chat_jid, agent_id, tool_name, title, summary,
                            description, content_json, ok, timestamp
                     FROM tool_executions
                     WHERE chat_jid = ?1
                     ORDER BY timestamp ASC, id ASC",
                )?;
                let v: Vec<StoredToolExecution> = stmt
                    .query_map(params![chat_jid], map_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                v
            };
            Ok(rows)
        })
    }

    /// Wipe all tool executions for a chat (used when its group_messages
    /// history is cleared).
    pub fn delete_tool_executions_for_jid(&self, chat_jid: &str) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM tool_executions WHERE chat_jid = ?1",
                params![chat_jid],
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::db::Db;

    #[test]
    fn round_trip_preserves_fields_and_order() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");

        let jid = "telegram:1";
        db.insert_tool_execution(
            jid,
            "main",
            "Read",
            "src/lib.rs",
            "Read 200 lines",
            "",
            r#"{"path":"src/lib.rs"}"#,
            true,
            "2026-05-19T10:00:00Z",
            500,
        )
        .unwrap();
        db.insert_tool_execution(
            jid,
            "main",
            "Bash",
            "ls",
            "0 exit",
            "List the crate root",
            r#"{"cmd":"ls"}"#,
            true,
            "2026-05-19T10:00:01Z",
            500,
        )
        .unwrap();
        db.insert_tool_execution(
            jid,
            "main",
            "Edit",
            "src/lib.rs",
            "edit failed",
            "",
            r#"{"error":"oops"}"#,
            false,
            "2026-05-19T10:00:02Z",
            500,
        )
        .unwrap();

        let rows = db.get_tool_executions(jid, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].tool_name, "Read");
        assert_eq!(rows[1].tool_name, "Bash");
        assert_eq!(rows[2].tool_name, "Edit");
        assert!(!rows[2].ok);
        assert_eq!(rows[0].content_json, r#"{"path":"src/lib.rs"}"#);
        // The step description round-trips, and a tool that carries none
        // reads back as empty rather than as the previous row's text.
        assert_eq!(rows[1].description, "List the crate root");
        assert_eq!(rows[0].description, "");

        // Limit returns the most recent N, still in ascending order.
        let recent = db.get_tool_executions(jid, Some(2)).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].tool_name, "Bash");
        assert_eq!(recent[1].tool_name, "Edit");

        // Per-chat isolation.
        let other = db.get_tool_executions("telegram:2", None).unwrap();
        assert!(other.is_empty());

        // Delete-by-jid.
        let n = db.delete_tool_executions_for_jid(jid).unwrap();
        assert_eq!(n, 3);
        assert!(db.get_tool_executions(jid, None).unwrap().is_empty());
    }

    #[test]
    fn insert_trims_to_per_chat_cap() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");

        let jid = "telegram:cap";
        for i in 0..10 {
            db.insert_tool_execution(
                jid,
                "main",
                "Read",
                &format!("file-{i}"),
                "",
                "",
                "{}",
                true,
                &format!("2026-05-19T10:00:{i:02}Z"),
                3, // default_limit = 3
            )
            .unwrap();
        }
        let rows = db.get_tool_executions(jid, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].title, "file-7");
        assert_eq!(rows[2].title, "file-9");
    }

    /// A database written before the `description` column existed must gain it
    /// on open, not fail on the first insert.
    ///
    /// This is the path a real install takes, and the one a unit test on a
    /// fresh schema can never exercise: `CREATE TABLE IF NOT EXISTS` is a
    /// no-op against the old shape, so only the guarded ALTER saves it.
    #[test]
    fn an_older_database_gains_the_step_description_column() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        // The table exactly as it shipped before this column.
        conn.execute_batch(
            r#"
            CREATE TABLE tool_executions (
              id            INTEGER PRIMARY KEY AUTOINCREMENT,
              chat_jid      TEXT NOT NULL,
              agent_id      TEXT NOT NULL DEFAULT 'main',
              tool_name     TEXT NOT NULL,
              title         TEXT NOT NULL DEFAULT '',
              summary       TEXT NOT NULL DEFAULT '',
              content_json  TEXT NOT NULL DEFAULT '{}',
              ok            INTEGER NOT NULL DEFAULT 1,
              timestamp     TEXT NOT NULL
            );
            INSERT INTO tool_executions
              (chat_jid, agent_id, tool_name, title, summary, content_json, ok, timestamp)
            VALUES ('telegram:1','main','Bash','ls','Completed','{}',1,'2026-05-19T10:00:00Z');
            "#,
        )
        .expect("old schema");

        crate::db::schema::apply_schema(&conn).expect("upgrade");

        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('tool_executions')")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(cols.iter().any(|c| c == "description"), "columns: {cols:?}");

        // The row that predates the column reads back as empty, which is what
        // makes the clients show the tool's verb rather than nothing at all.
        let existing: String = conn
            .query_row(
                "SELECT description FROM tool_executions WHERE tool_name = 'Bash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(existing, "");

        // And a new write through the upgraded table keeps its description.
        conn.execute(
            "INSERT INTO tool_executions
               (chat_jid, agent_id, tool_name, title, summary, description,
                content_json, ok, timestamp)
             VALUES ('telegram:1','main','Bash','pwd','Completed',
                     'Find the working directory','{}',1,'2026-05-19T10:00:01Z')",
            [],
        )
        .unwrap();
        let fresh: String = conn
            .query_row(
                "SELECT description FROM tool_executions WHERE tool_name = 'Bash' AND title = 'pwd'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fresh, "Find the working directory");
    }
}

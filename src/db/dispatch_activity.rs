//! Persistence for `dispatch:activity` entries so the UI can replay sub-agent
//! tool calls and messages after a page reload. Keyed by dispatch `task_id`.

use anyhow::Result;
use rusqlite::params;

/// One persisted dispatch activity entry.
#[derive(Debug, Clone)]
pub struct StoredDispatchActivity {
    pub id: i64,
    pub task_id: String,
    pub parent_id: String,
    pub entry_type: String,
    pub tool_name: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub content_json: Option<String>,
    pub ok: Option<bool>,
    pub text: Option<String>,
    pub ts: String,
}

/// Max entries per task_id. Oldest rows are FIFO-trimmed on insert.
const MAX_ENTRIES_PER_TASK: i64 = 500;

impl super::Db {
    /// Insert a dispatch activity entry. FIFO-trims to [`MAX_ENTRIES_PER_TASK`].
    pub fn insert_dispatch_activity(
        &self,
        task_id: &str,
        parent_id: &str,
        entry_type: &str,
        tool_name: Option<&str>,
        title: Option<&str>,
        summary: Option<&str>,
        content_json: Option<&str>,
        ok: Option<bool>,
        text: Option<&str>,
        ts: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                r#"
                INSERT INTO dispatch_activity
                  (task_id, parent_id, entry_type, tool_name, title,
                   summary, content_json, ok, text, ts)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                "#,
                params![
                    task_id,
                    parent_id,
                    entry_type,
                    tool_name,
                    title,
                    summary,
                    content_json,
                    ok,
                    text,
                    ts,
                ],
            )?;

            // FIFO trim
            let count: i64 = c.query_row(
                "SELECT COUNT(*) FROM dispatch_activity WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )?;
            if count > MAX_ENTRIES_PER_TASK {
                c.execute(
                    r#"
                    DELETE FROM dispatch_activity
                    WHERE id IN (
                      SELECT id FROM dispatch_activity
                      WHERE task_id = ?1
                      ORDER BY id ASC
                      LIMIT ?2
                    )
                    "#,
                    params![task_id, count - MAX_ENTRIES_PER_TASK],
                )?;
            }
            Ok(())
        })
    }

    /// Load all activity entries for a set of task IDs (e.g. all tasks in an
    /// active parent). Ordered by id ASC (chronological).
    pub fn get_dispatch_activity(&self, task_ids: &[&str]) -> Result<Vec<StoredDispatchActivity>> {
        if task_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.with_conn(|c| {
            // Build IN clause with positional params
            let placeholders: String = task_ids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"
                SELECT id, task_id, parent_id, entry_type, tool_name,
                       title, summary, content_json, ok, text, ts
                FROM dispatch_activity
                WHERE task_id IN ({placeholders})
                ORDER BY id ASC
                "#
            );
            let mut stmt = c.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> =
                task_ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let rows = stmt
                .query_map(params.as_slice(), |r| {
                    Ok(StoredDispatchActivity {
                        id: r.get(0)?,
                        task_id: r.get(1)?,
                        parent_id: r.get(2)?,
                        entry_type: r.get(3)?,
                        tool_name: r.get(4)?,
                        title: r.get(5)?,
                        summary: r.get(6)?,
                        content_json: r.get(7)?,
                        ok: r.get(8)?,
                        text: r.get(9)?,
                        ts: r.get(10)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Delete all activity for a parent's tasks (cleanup when parent is removed).
    pub fn delete_dispatch_activity_for_parent(&self, parent_id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "DELETE FROM dispatch_activity WHERE parent_id = ?1",
                params![parent_id],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Db;

    fn test_db() -> Db {
        Db::open_in_memory(&crate::config::Config::from_env()).unwrap()
    }

    #[test]
    fn insert_and_query() {
        let db = test_db();
        db.insert_dispatch_activity(
            "d-001",
            "",
            "tool",
            Some("Read"),
            Some("src/main.rs"),
            Some("100 lines"),
            None,
            Some(true),
            None,
            "2026-06-05T10:00:00Z",
        )
        .unwrap();
        db.insert_dispatch_activity(
            "d-001",
            "",
            "message",
            None,
            None,
            None,
            None,
            None,
            Some("Analyzing the file..."),
            "2026-06-05T10:00:01Z",
        )
        .unwrap();
        db.insert_dispatch_activity(
            "d-002",
            "",
            "tool",
            Some("Bash"),
            Some("cargo test"),
            None,
            None,
            Some(true),
            None,
            "2026-06-05T10:00:02Z",
        )
        .unwrap();

        let rows = db.get_dispatch_activity(&["d-001"]).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entry_type, "tool");
        assert_eq!(rows[0].tool_name.as_deref(), Some("Read"));
        assert_eq!(rows[1].entry_type, "message");
        assert_eq!(rows[1].text.as_deref(), Some("Analyzing the file..."));

        // Multi-task query
        let all = db.get_dispatch_activity(&["d-001", "d-002"]).unwrap();
        assert_eq!(all.len(), 3);

        // Empty query
        let empty = db.get_dispatch_activity(&[]).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn fifo_trim() {
        let db = test_db();
        // Insert more than MAX_ENTRIES_PER_TASK (500) and verify trim
        for i in 0..510 {
            db.insert_dispatch_activity(
                "d-trim",
                "",
                "tool",
                Some("Read"),
                Some(&format!("file-{i}.rs")),
                None,
                None,
                Some(true),
                None,
                &format!("2026-06-05T10:{:02}:{:02}Z", i / 60, i % 60),
            )
            .unwrap();
        }
        let rows = db.get_dispatch_activity(&["d-trim"]).unwrap();
        assert_eq!(rows.len(), 500, "should be trimmed to 500");
        // Oldest entries (0-9) should be gone; newest (10-509) remain
        assert_eq!(rows[0].title.as_deref(), Some("file-10.rs"));
    }
}

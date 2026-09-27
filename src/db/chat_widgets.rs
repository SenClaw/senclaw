//! Persistence for one-way chat widgets (`emit_widget`) so the chat UI can
//! replay them after a page reload. Mirrors [`super::tool_executions`] —
//! FIFO-trimmed per chat on insert so a chatty agent can't grow this table
//! unboundedly.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

/// One persisted chat widget. Maps 1:1 to the `chat:widget` WebSocket frame
/// and to the history-replay `{ role: "widget" }` row.
#[derive(Debug, Clone)]
pub struct StoredChatWidget {
    /// `widget-<uuid>` — stable id shared with the live frame.
    pub id: String,
    pub chat_jid: String,
    /// Serialised `WidgetSpec` (raw JSON text: `{kind,title,data}`).
    pub widget_json: String,
    pub created_at: String,
}

impl super::Db {
    /// Insert a chat widget row and FIFO-trim the chat to its retention limit
    /// (same cap as `group_messages`, falling back to `default_limit`).
    pub fn insert_chat_widget(
        &self,
        id: &str,
        chat_jid: &str,
        widget_json: &str,
        created_at: &str,
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
                INSERT OR REPLACE INTO chat_widgets
                  (id, chat_jid, widget_json, created_at)
                VALUES (?1,?2,?3,?4)
                "#,
                params![id, chat_jid, widget_json, created_at],
            )?;

            c.execute(
                r#"
                DELETE FROM chat_widgets
                WHERE chat_jid = ?1
                  AND id NOT IN (
                    SELECT id FROM chat_widgets
                    WHERE chat_jid = ?1
                    ORDER BY created_at DESC, id DESC
                    LIMIT ?2
                  )
                "#,
                params![chat_jid, limit],
            )?;
            Ok(())
        })
    }

    /// Fetch chat widgets for a chat in chronological order (oldest first).
    /// `limit` caps the row count when set; `None` means "all rows".
    pub fn get_chat_widgets(
        &self,
        chat_jid: &str,
        limit: Option<u32>,
    ) -> Result<Vec<StoredChatWidget>> {
        self.with_conn(|c| {
            let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<StoredChatWidget> {
                Ok(StoredChatWidget {
                    id: r.get(0)?,
                    chat_jid: r.get(1)?,
                    widget_json: r.get(2)?,
                    created_at: r.get(3)?,
                })
            };

            let rows: Vec<StoredChatWidget> = if let Some(lim) = limit {
                let mut stmt = c.prepare(
                    "SELECT id, chat_jid, widget_json, created_at
                     FROM chat_widgets
                     WHERE chat_jid = ?1
                     ORDER BY created_at DESC, id DESC
                     LIMIT ?2",
                )?;
                let mut v: Vec<StoredChatWidget> = stmt
                    .query_map(params![chat_jid, lim as i64], map_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                v.reverse();
                v
            } else {
                let mut stmt = c.prepare(
                    "SELECT id, chat_jid, widget_json, created_at
                     FROM chat_widgets
                     WHERE chat_jid = ?1
                     ORDER BY created_at ASC, id ASC",
                )?;
                let v: Vec<StoredChatWidget> = stmt
                    .query_map(params![chat_jid], map_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                v
            };
            Ok(rows)
        })
    }

    /// Wipe all chat widgets for a chat (used when its group_messages history
    /// is cleared).
    pub fn delete_chat_widgets_for_jid(&self, chat_jid: &str) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM chat_widgets WHERE chat_jid = ?1",
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
    fn widget_round_trip_preserves_order_and_fields() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");

        let jid = "telegram:1";
        db.insert_chat_widget(
            "widget-a",
            jid,
            r#"{"kind":"chart","data":{}}"#,
            "2026-05-19T10:00:00Z",
            500,
        )
        .unwrap();
        db.insert_chat_widget(
            "widget-b",
            jid,
            r#"{"kind":"weather","data":{}}"#,
            "2026-05-19T10:00:01Z",
            500,
        )
        .unwrap();

        let rows = db.get_chat_widgets(jid, None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "widget-a");
        assert_eq!(rows[1].id, "widget-b");
        assert_eq!(rows[1].widget_json, r#"{"kind":"weather","data":{}}"#);

        // Per-chat isolation.
        assert!(db.get_chat_widgets("telegram:2", None).unwrap().is_empty());

        // Delete-by-jid.
        assert_eq!(db.delete_chat_widgets_for_jid(jid).unwrap(), 2);
        assert!(db.get_chat_widgets(jid, None).unwrap().is_empty());
    }

    #[test]
    fn widget_insert_trims_to_cap() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");
        let jid = "telegram:cap";
        for i in 0..10 {
            db.insert_chat_widget(
                &format!("widget-{i}"),
                jid,
                "{}",
                &format!("2026-05-19T10:00:{i:02}Z"),
                3,
            )
            .unwrap();
        }
        let rows = db.get_chat_widgets(jid, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, "widget-7");
        assert_eq!(rows[2].id, "widget-9");
    }
}

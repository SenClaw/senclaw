//! Persistence for ephemeral chat events (agent state transitions,
//! permission and question requests + their resolutions). Replayed on
//! subscribe via a `chat:history` frame so the Web UI can rebuild
//! mid-flight interactions after a page reload.

use anyhow::Result;
use rusqlite::params;

#[derive(Debug, Clone)]
pub struct StoredChatEvent {
    pub id: i64,
    pub chat_jid: String,
    pub event_type: String,
    pub request_id: Option<String>,
    pub payload_json: String,
    pub timestamp: String,
}

impl super::Db {
    pub fn insert_chat_event(
        &self,
        chat_jid: &str,
        event_type: &str,
        request_id: Option<&str>,
        payload_json: &str,
        timestamp: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                r#"
                INSERT INTO chat_events (chat_jid, event_type, request_id, payload, timestamp)
                VALUES (?1, ?2, ?3, ?4, ?5)
                "#,
                params![chat_jid, event_type, request_id, payload_json, timestamp],
            )?;
            Ok(())
        })
    }

    /// Fetch chat events for a chat in chronological order. When `limit` is
    /// set, returns the most recent N rows but in ascending order so the
    /// caller can replay them in time order.
    pub fn get_chat_events(
        &self,
        chat_jid: &str,
        limit: Option<u32>,
    ) -> Result<Vec<StoredChatEvent>> {
        self.with_conn(|c| {
            let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<StoredChatEvent> {
                Ok(StoredChatEvent {
                    id: r.get(0)?,
                    chat_jid: r.get(1)?,
                    event_type: r.get(2)?,
                    request_id: r.get(3)?,
                    payload_json: r.get(4)?,
                    timestamp: r.get(5)?,
                })
            };
            let rows: Vec<StoredChatEvent> = if let Some(lim) = limit {
                let mut stmt = c.prepare(
                    "SELECT id, chat_jid, event_type, request_id, payload, timestamp
                     FROM chat_events
                     WHERE chat_jid = ?1
                     ORDER BY timestamp DESC, id DESC
                     LIMIT ?2",
                )?;
                let mut v: Vec<StoredChatEvent> = stmt
                    .query_map(params![chat_jid, lim as i64], map_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                v.reverse();
                v
            } else {
                let mut stmt = c.prepare(
                    "SELECT id, chat_jid, event_type, request_id, payload, timestamp
                     FROM chat_events
                     WHERE chat_jid = ?1
                     ORDER BY timestamp ASC, id ASC",
                )?;
                let v: Vec<StoredChatEvent> = stmt
                    .query_map(params![chat_jid], map_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                v
            };
            Ok(rows)
        })
    }

    pub fn delete_chat_events_for_jid(&self, chat_jid: &str) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM chat_events WHERE chat_jid = ?1",
                params![chat_jid],
            )?)
        })
    }

    /// Boot-time cleanup of unresolved permission/question requests older
    /// than `cutoff` ISO-8601 timestamp. The chat UI replays unresolved
    /// `permission:request` / `question:request` rows on subscribe, so if
    /// the agent crashed (or the user just restarted) mid-prompt, the
    /// next session resurrects a ghost approval the agent has long since
    /// forgotten about. We delete the request row AND any sibling
    /// resolution rows tagged with the same request_id.
    ///
    /// Returns the number of `chat_events` rows removed.
    pub fn cleanup_stale_pending_interactions(&self, cutoff_iso: &str) -> Result<usize> {
        self.with_conn(|c| {
            // Find request_ids of unresolved requests older than cutoff.
            // Unresolved = no sibling row with `:resolved` suffix exists.
            let mut stmt = c.prepare(
                "SELECT request_id FROM chat_events e
                 WHERE e.event_type IN ('permission:request', 'question:request')
                   AND e.timestamp < ?1
                   AND e.request_id IS NOT NULL
                   AND NOT EXISTS (
                     SELECT 1 FROM chat_events r
                     WHERE r.request_id = e.request_id
                       AND (r.event_type = 'permission:resolved'
                            OR r.event_type = 'question:resolved')
                   )",
            )?;
            let ids: Vec<String> = stmt
                .query_map(params![cutoff_iso], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
            drop(stmt);
            if ids.is_empty() {
                return Ok(0);
            }
            // Delete every event tied to those request_ids — keeps the DB
            // tidy even when an orphan resolved row sneaks in later.
            let mut total = 0usize;
            for id in &ids {
                total += c.execute("DELETE FROM chat_events WHERE request_id = ?1", params![id])?;
            }
            Ok(total)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::db::Db;

    #[test]
    fn round_trip_preserves_order_and_request_id() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");

        let jid = "tg:1";
        db.insert_chat_event(
            jid,
            "agent:state",
            None,
            r#"{"state":"idle"}"#,
            "2026-05-19T10:00:00Z",
        )
        .unwrap();
        db.insert_chat_event(
            jid,
            "permission:request",
            Some("req-1"),
            r#"{"toolName":"Bash"}"#,
            "2026-05-19T10:00:01Z",
        )
        .unwrap();
        db.insert_chat_event(
            jid,
            "permission:resolved",
            Some("req-1"),
            r#"{"key":"allow"}"#,
            "2026-05-19T10:00:02Z",
        )
        .unwrap();

        let rows = db.get_chat_events(jid, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].event_type, "agent:state");
        assert_eq!(rows[1].event_type, "permission:request");
        assert_eq!(rows[1].request_id.as_deref(), Some("req-1"));
        assert_eq!(rows[2].event_type, "permission:resolved");

        // Limit returns most-recent N but in ascending order.
        let recent = db.get_chat_events(jid, Some(2)).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].event_type, "permission:request");
        assert_eq!(recent[1].event_type, "permission:resolved");

        // Per-jid isolation.
        assert!(db.get_chat_events("tg:other", None).unwrap().is_empty());

        // Delete.
        assert_eq!(db.delete_chat_events_for_jid(jid).unwrap(), 3);
        assert!(db.get_chat_events(jid, None).unwrap().is_empty());
    }

    #[test]
    fn cleanup_stale_pending_drops_unresolved_old_requests_only() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");
        let jid = "web:main";

        // Old, unresolved → should be deleted.
        db.insert_chat_event(
            jid,
            "permission:request",
            Some("old-1"),
            r#"{"toolName":"Write"}"#,
            "2026-04-01T00:00:00Z",
        )
        .unwrap();
        // Old, resolved → KEEP (request has matching resolution).
        db.insert_chat_event(
            jid,
            "permission:request",
            Some("old-2"),
            r#"{"toolName":"Bash"}"#,
            "2026-04-01T00:00:01Z",
        )
        .unwrap();
        db.insert_chat_event(
            jid,
            "permission:resolved",
            Some("old-2"),
            r#"{"key":"allow"}"#,
            "2026-04-01T00:00:02Z",
        )
        .unwrap();
        // Recent, unresolved → KEEP (newer than cutoff).
        db.insert_chat_event(
            jid,
            "permission:request",
            Some("new-1"),
            r#"{"toolName":"Write"}"#,
            "2026-05-20T17:00:00Z",
        )
        .unwrap();
        // Unrelated event → untouched regardless of age.
        db.insert_chat_event(
            jid,
            "agent:state",
            None,
            r#"{"state":"idle"}"#,
            "2026-04-01T00:00:03Z",
        )
        .unwrap();

        // Cutoff between the old batch and the recent one.
        let removed = db
            .cleanup_stale_pending_interactions("2026-05-01T00:00:00Z")
            .unwrap();
        // Only `old-1` is removed (1 row). `old-2` is resolved so it stays.
        assert_eq!(removed, 1);

        let remaining = db.get_chat_events(jid, None).unwrap();
        // Should have: old-2 request, old-2 resolved, new-1 request, agent:state
        assert_eq!(remaining.len(), 4);
        assert!(remaining
            .iter()
            .all(|r| r.request_id.as_deref() != Some("old-1")));
    }
}

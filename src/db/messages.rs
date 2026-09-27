use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use crate::types::StoredMessage;

use super::rows::row_to_message;

/// group_messages timestamps come in two formats: RFC3339 (channel adapters,
/// UTC) and host-local `YYYY-MM-DD HH:MM:SS` (web-sent messages). Parse both.
fn parse_message_ts_ms(s: &str) -> Option<i64> {
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp_millis());
    }
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|d| d.timestamp_millis())
}

impl super::Db {
    // ============================================================
    // Messages (channel_messages + group_messages)
    // ============================================================

    /// Insert a message and FIFO-trim the chat to its retention limit.
    pub fn insert_message(&self, msg: &StoredMessage, default_limit: u32) -> Result<()> {
        self.with_conn(|c| {
            let limit: i64 = c
                .query_row(
                    "SELECT max_messages FROM groups WHERE jid = ?1",
                    params![msg.chat_jid],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten()
                .unwrap_or(default_limit as i64);

            c.execute(
                r#"
                INSERT OR IGNORE INTO channel_messages
                  (message_id, chat_jid, sender_jid, sender_name, content,
                   timestamp, is_from_me, is_bot_reply, reply_to_id, media_type)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                "#,
                params![
                    msg.message_id,
                    msg.chat_jid,
                    msg.sender_jid,
                    msg.sender_name,
                    msg.content,
                    msg.timestamp,
                    msg.is_from_me as i64,
                    msg.is_bot_reply as i64,
                    msg.reply_to_id,
                    msg.media_type,
                ],
            )?;

            c.execute(
                r#"
                DELETE FROM channel_messages
                WHERE chat_jid = ?1
                  AND message_id NOT IN (
                    SELECT message_id FROM channel_messages
                    WHERE chat_jid = ?1
                    ORDER BY timestamp DESC
                    LIMIT ?2
                  )
                "#,
                params![msg.chat_jid, limit],
            )?;
            Ok(())
        })
    }

    pub fn get_messages(&self, chat_jid: &str, since: Option<&str>) -> Result<Vec<StoredMessage>> {
        self.with_conn(|c| {
            let rows: Vec<rusqlite::Result<Result<StoredMessage>>> = if let Some(since) = since {
                let mut stmt = c.prepare(
                    "SELECT * FROM channel_messages
                     WHERE chat_jid = ?1 AND timestamp > ?2
                     ORDER BY timestamp ASC",
                )?;
                let v: Vec<_> = stmt
                    .query_map(params![chat_jid, since], |r| Ok(row_to_message(r)))?
                    .collect();
                v
            } else {
                let mut stmt = c.prepare(
                    "SELECT * FROM channel_messages
                     WHERE chat_jid = ?1
                     ORDER BY timestamp ASC",
                )?;
                let v: Vec<_> = stmt
                    .query_map(params![chat_jid], |r| Ok(row_to_message(r)))?
                    .collect();
                v
            };
            rows.into_iter()
                .map(|r| r.map_err(anyhow::Error::from).and_then(|inner| inner))
                .collect()
        })
    }

    pub fn get_messages_paginated(
        &self,
        chat_jid: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<StoredMessage>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM channel_messages
                 WHERE chat_jid = ?1
                 ORDER BY timestamp DESC
                 LIMIT ?2 OFFSET ?3",
            )?;
            let rows = stmt
                .query_map(params![chat_jid, limit as i64, offset as i64], |r| {
                    Ok(row_to_message(r))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut results = Vec::new();
            for r in rows {
                results.push(r?);
            }
            Ok(results)
        })
    }

    /// Delete all messages for a chat JID.
    pub fn delete_messages_for_jid(&self, chat_jid: &str) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM channel_messages WHERE chat_jid = ?1",
                params![chat_jid],
            )?)
        })
    }

    pub fn count_messages(&self, chat_jid: &str) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM channel_messages WHERE chat_jid = ?1",
                params![chat_jid],
                |r| r.get::<_, usize>(0),
            )?)
        })
    }

    // ============================================================
    // Group messages (conversation history: user + bot responses)
    // ============================================================

    pub fn insert_group_message(&self, msg: &StoredMessage, default_limit: u32) -> Result<()> {
        self.with_conn(|c| {
            let limit: i64 = c
                .query_row(
                    "SELECT max_messages FROM groups WHERE jid = ?1",
                    params![msg.chat_jid],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten()
                .unwrap_or(default_limit as i64);

            c.execute(
                r#"
                INSERT OR IGNORE INTO group_messages
                  (message_id, chat_jid, sender_jid, sender_name, content,
                   timestamp, is_from_me, is_bot_reply, reply_to_id, media_type, attachments)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                "#,
                params![
                    msg.message_id,
                    msg.chat_jid,
                    msg.sender_jid,
                    msg.sender_name,
                    msg.content,
                    msg.timestamp,
                    msg.is_from_me as i64,
                    msg.is_bot_reply as i64,
                    msg.reply_to_id,
                    msg.media_type,
                    msg.attachments,
                ],
            )?;

            c.execute(
                r#"
                DELETE FROM group_messages
                WHERE chat_jid = ?1
                  AND message_id NOT IN (
                    SELECT message_id FROM group_messages
                    WHERE chat_jid = ?1
                    ORDER BY timestamp DESC
                    LIMIT ?2
                  )
                "#,
                params![msg.chat_jid, limit],
            )?;
            Ok(())
        })
    }

    /// Last message/response timestamp (ms since epoch) per chat, from
    /// group_messages. Powers the sidebar "recent activity" sort.
    pub fn last_activity_per_group(&self) -> Result<std::collections::HashMap<String, i64>> {
        self.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT chat_jid, MAX(timestamp) FROM group_messages GROUP BY chat_jid")?;
            let rows: Vec<(String, Option<String>)> = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let mut map = std::collections::HashMap::new();
            for (jid, ts) in rows {
                if let Some(ms) = ts.as_deref().and_then(parse_message_ts_ms) {
                    map.insert(jid, ms);
                }
            }
            Ok(map)
        })
    }

    pub fn get_group_messages(
        &self,
        chat_jid: &str,
        since: Option<&str>,
    ) -> Result<Vec<StoredMessage>> {
        self.with_conn(|c| {
            let rows: Vec<rusqlite::Result<Result<StoredMessage>>> = if let Some(since) = since {
                let mut stmt = c.prepare(
                    "SELECT * FROM group_messages
                     WHERE chat_jid = ?1 AND timestamp > ?2
                     ORDER BY timestamp ASC",
                )?;
                let v: Vec<_> = stmt
                    .query_map(params![chat_jid, since], |r| Ok(row_to_message(r)))?
                    .collect();
                v
            } else {
                let mut stmt = c.prepare(
                    "SELECT * FROM group_messages
                     WHERE chat_jid = ?1
                     ORDER BY timestamp ASC",
                )?;
                let v: Vec<_> = stmt
                    .query_map(params![chat_jid], |r| Ok(row_to_message(r)))?
                    .collect();
                v
            };
            rows.into_iter()
                .map(|r| r.map_err(anyhow::Error::from).and_then(|inner| inner))
                .collect()
        })
    }

    pub fn get_group_messages_paginated(
        &self,
        chat_jid: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<StoredMessage>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM group_messages
                 WHERE chat_jid = ?1
                 ORDER BY timestamp DESC
                 LIMIT ?2 OFFSET ?3",
            )?;
            let rows = stmt
                .query_map(params![chat_jid, limit as i64, offset as i64], |r| {
                    Ok(row_to_message(r))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut results = Vec::new();
            for r in rows {
                results.push(r?);
            }
            Ok(results)
        })
    }

    /// Messages for a chat strictly newer than `after_ms` (epoch millis),
    /// oldest → newest, capped to the newest `limit` rows. Each row is paired
    /// with its parsed epoch-ms timestamp so callers get a stable numeric
    /// cursor. Timestamps are stored as strings in two formats (RFC3339 and
    /// host-local `YYYY-MM-DD HH:MM:SS`), so filtering/sorting happens in Rust
    /// via `parse_message_ts_ms` rather than SQL string comparison — string
    /// ORDER BY mis-sorts across the two formats. Row counts per chat are
    /// FIFO-trimmed to `max_messages`, so the full-chat scan stays bounded.
    pub fn get_group_messages_after_ms(
        &self,
        chat_jid: &str,
        after_ms: i64,
        limit: u32,
    ) -> Result<Vec<(StoredMessage, i64)>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT * FROM group_messages WHERE chat_jid = ?1")?;
            let rows = stmt
                .query_map(params![chat_jid], |r| Ok(row_to_message(r)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut out = Vec::new();
            for r in rows {
                let m = r?;
                if let Some(ms) = parse_message_ts_ms(&m.timestamp) {
                    if ms > after_ms {
                        out.push((m, ms));
                    }
                }
            }
            out.sort_by(|a, b| {
                a.1.cmp(&b.1)
                    .then_with(|| a.0.message_id.cmp(&b.0.message_id))
            });
            if out.len() > limit as usize {
                out.drain(..out.len() - limit as usize);
            }
            Ok(out)
        })
    }

    pub fn delete_group_messages_for_jid(&self, chat_jid: &str) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM group_messages WHERE chat_jid = ?1",
                params![chat_jid],
            )?)
        })
    }

    pub fn count_group_messages(&self, chat_jid: &str) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM group_messages WHERE chat_jid = ?1",
                params![chat_jid],
                |r| r.get::<_, usize>(0),
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::db::Db;
    use crate::types::StoredMessage;

    fn msg(id: &str, jid: &str, ts: &str) -> StoredMessage {
        StoredMessage {
            message_id: id.into(),
            chat_jid: jid.into(),
            sender_jid: String::new(),
            sender_name: String::new(),
            content: "hi".into(),
            timestamp: ts.into(),
            is_from_me: false,
            is_bot_reply: false,
            reply_to_id: None,
            media_type: None,
            attachments: None,
        }
    }

    #[test]
    fn last_activity_handles_both_timestamp_formats() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");

        // RFC3339 (channel adapters) — newest of the two wins.
        db.insert_group_message(&msg("m1", "tg:1", "2026-07-01T08:00:00+00:00"), 100)
            .unwrap();
        db.insert_group_message(&msg("m2", "tg:1", "2026-07-02T09:30:00+00:00"), 100)
            .unwrap();
        // Host-local "YYYY-MM-DD HH:MM:SS" (web-sent messages).
        db.insert_group_message(&msg("m3", "web:x:abc", "2026-07-02 10:00:00"), 100)
            .unwrap();

        let map = db.last_activity_per_group().unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2026-07-02T09:30:00+00:00")
            .unwrap()
            .timestamp_millis();
        assert_eq!(map.get("tg:1"), Some(&expected));
        // Local-format timestamp parses to a real epoch (exact value depends
        // on the host timezone — just require presence and sanity).
        let web_ms = *map.get("web:x:abc").expect("web chat present");
        assert!(
            web_ms > 1_700_000_000_000,
            "epoch ms expected, got {web_ms}"
        );
    }

    #[test]
    fn after_ms_filters_sorts_and_caps_mixed_formats() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");

        let jid = "app:c1:user:mobile-app";
        db.insert_group_message(&msg("m1", jid, "2026-07-01T08:00:00+00:00"), 100)
            .unwrap();
        db.insert_group_message(&msg("m2", jid, "2026-07-02T09:00:00+00:00"), 100)
            .unwrap();
        db.insert_group_message(&msg("m3", jid, "2026-07-02T10:00:00+00:00"), 100)
            .unwrap();

        // No cursor (−1): everything, oldest → newest, with parsed epoch ms.
        let all = db.get_group_messages_after_ms(jid, -1, 100).unwrap();
        assert_eq!(
            all.iter()
                .map(|(m, _)| m.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["m1", "m2", "m3"]
        );
        assert!(all.windows(2).all(|w| w[0].1 <= w[1].1));

        // Strictly-after cursor: only messages newer than m2.
        let m2_ms = chrono::DateTime::parse_from_rfc3339("2026-07-02T09:00:00+00:00")
            .unwrap()
            .timestamp_millis();
        let delta = db.get_group_messages_after_ms(jid, m2_ms, 100).unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].0.message_id, "m3");

        // Limit keeps the NEWEST rows.
        let capped = db.get_group_messages_after_ms(jid, -1, 2).unwrap();
        assert_eq!(
            capped
                .iter()
                .map(|(m, _)| m.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["m2", "m3"]
        );
    }
}

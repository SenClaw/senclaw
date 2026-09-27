use anyhow::Result;
use rusqlite::{params, OptionalExtension};

impl super::Db {
    // ============================================================
    // Router state
    // ============================================================

    pub fn get_router_state(&self, key: &str) -> Result<Option<String>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT value FROM router_state WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })
    }

    pub fn set_router_state(&self, key: &str, value: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO router_state (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
    }

    /// Forget a key. Distinct from writing an empty value: a caller that
    /// treats "absent" as "fall back to something else" needs the row gone.
    pub fn delete_router_state(&self, key: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM router_state WHERE key = ?1", params![key])?;
            Ok(())
        })
    }

    pub fn get_last_agent_timestamp(&self, chat_jid: &str) -> Result<Option<String>> {
        self.get_router_state(&format!("lastAgent:{chat_jid}"))
    }

    pub fn set_last_agent_timestamp(&self, chat_jid: &str, timestamp: &str) -> Result<()> {
        self.set_router_state(&format!("lastAgent:{chat_jid}"), timestamp)
    }

    pub fn delete_agent_timestamp(&self, chat_jid: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "DELETE FROM router_state WHERE key = ?1",
                params![format!("lastAgent:{chat_jid}")],
            )?;
            Ok(())
        })
    }
}

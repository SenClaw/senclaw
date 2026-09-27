use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use crate::types::Channel;

use super::rows::row_to_channel;

impl super::Db {
    // ============================================================
    // Channels
    // ============================================================

    pub fn insert_channel(
        &self,
        platform_type: &str,
        name: &str,
        credentials_json: &str,
        now: &str,
    ) -> Result<i64> {
        self.with_conn(|c| {
            c.execute("INSERT INTO channels (platform_type, name, credentials_json, created_at, updated_at) VALUES (?1,?2,?3,?4,?4)", params![platform_type, name, credentials_json, now])?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn get_channel(&self, id: i64) -> Result<Option<Channel>> {
        self.with_conn(|c| {
            c.query_row("SELECT * FROM channels WHERE id = ?1", params![id], |r| {
                Ok(row_to_channel(r))
            })
            .optional()?
            .transpose()
        })
    }

    pub fn find_channels_by_platform(&self, platform_type: &str) -> Result<Vec<Channel>> {
        self.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT * FROM channels WHERE platform_type=?1 ORDER BY id")?;
            let rows: Vec<_> = stmt
                .query_map(params![platform_type], |r| Ok(row_to_channel(r)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect::<Result<Vec<_>>>()
        })
    }

    pub fn list_channels(&self) -> Result<Vec<Channel>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT * FROM channels ORDER BY id")?;
            let rows: Vec<_> = stmt
                .query_map([], |r| Ok(row_to_channel(r)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect::<Result<Vec<_>>>()
        })
    }

    pub fn delete_channel(&self, id: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM channels WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    pub fn update_channel(
        &self,
        id: i64,
        name: Option<&str>,
        credentials_json: Option<&str>,
        now: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            if let Some(n) = name {
                c.execute(
                    "UPDATE channels SET name=?1,updated_at=?2 WHERE id=?3",
                    params![n, now, id],
                )?;
            }
            if let Some(creds) = credentials_json {
                c.execute(
                    "UPDATE channels SET credentials_json=?1,updated_at=?2 WHERE id=?3",
                    params![creds, now, id],
                )?;
            }
            Ok(())
        })
    }
}

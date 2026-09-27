use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use crate::types::{Binding, BindingWithRelations};

use super::rows::{row_to_binding, row_to_binding_with_relations};

impl super::Db {
    // ============================================================
    // Bindings
    // ============================================================

    pub fn insert_binding(
        &self,
        jid: Option<&str>,
        agent_id: i64,
        channel_id: i64,
        bot_token_override: Option<&str>,
        max_messages: Option<u32>,
        now: &str,
    ) -> Result<i64> {
        self.with_conn(|c| {
            c.execute("INSERT INTO bindings (jid,agent_id,channel_id,bot_token_override,max_messages,created_at) VALUES (?1,?2,?3,?4,?5,?6)", params![jid,agent_id,channel_id,bot_token_override,max_messages,now])?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn get_binding(&self, id: i64) -> Result<Option<Binding>> {
        self.with_conn(|c| {
            c.query_row("SELECT * FROM bindings WHERE id = ?1", params![id], |r| {
                Ok(row_to_binding(r))
            })
            .optional()?
            .transpose()
        })
    }

    pub fn get_binding_by_jid(&self, jid: &str) -> Result<Option<Binding>> {
        self.with_conn(|c| {
            c.query_row("SELECT * FROM bindings WHERE jid = ?1", params![jid], |r| {
                Ok(row_to_binding(r))
            })
            .optional()?
            .transpose()
        })
    }

    pub fn get_binding_with_relations(&self, jid: &str) -> Result<Option<BindingWithRelations>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT b.id,b.jid,b.agent_id,b.channel_id,b.is_admin,b.bot_token_override,b.max_messages,b.last_active,b.created_at, a.id,a.folder,a.name,a.requires_trigger,a.allowed_tools,a.allowed_paths,a.allowed_work_dirs,a.core_prompt,a.model_id,a.created_at,a.updated_at, ch.id,ch.platform_type,ch.name,ch.credentials_json,ch.connection_state,ch.created_at,ch.updated_at FROM bindings b JOIN agents a ON b.agent_id=a.id JOIN channels ch ON b.channel_id=ch.id WHERE b.jid=?1",
                params![jid],
                |r| Ok(row_to_binding_with_relations(r)),
            ).optional()?.transpose()
        })
    }

    pub fn get_pending_bindings_for_channel(&self, channel_id: i64) -> Result<Vec<Binding>> {
        self.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT * FROM bindings WHERE channel_id=?1 AND jid IS NULL")?;
            let rows: Vec<_> = stmt
                .query_map(params![channel_id], |r| Ok(row_to_binding(r)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect::<Result<Vec<_>>>()
        })
    }

    pub fn complete_pending_binding(&self, binding_id: i64, jid: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE bindings SET jid=?1 WHERE id=?2 AND jid IS NULL",
                params![jid, binding_id],
            )?;
            Ok(())
        })
    }

    pub fn list_bindings(&self) -> Result<Vec<Binding>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT * FROM bindings ORDER BY id")?;
            let rows: Vec<_> = stmt
                .query_map([], |r| Ok(row_to_binding(r)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect::<Result<Vec<_>>>()
        })
    }

    pub fn list_bindings_with_relations(&self) -> Result<Vec<BindingWithRelations>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT b.id,b.jid,b.agent_id,b.channel_id,b.is_admin,b.bot_token_override,b.max_messages,b.last_active,b.created_at, a.id,a.folder,a.name,a.requires_trigger,a.allowed_tools,a.allowed_paths,a.allowed_work_dirs,a.core_prompt,a.model_id,a.created_at,a.updated_at, ch.id,ch.platform_type,ch.name,ch.credentials_json,ch.connection_state,ch.created_at,ch.updated_at FROM bindings b JOIN agents a ON b.agent_id=a.id JOIN channels ch ON b.channel_id=ch.id ORDER BY b.id")?;
            let rows: Vec<_> = stmt.query_map([], |r| Ok(row_to_binding_with_relations(r)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect::<Result<Vec<_>>>()
        })
    }

    pub fn list_bindings_for_channel(&self, channel_id: i64) -> Result<Vec<BindingWithRelations>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT b.id,b.jid,b.agent_id,b.channel_id,b.is_admin,b.bot_token_override,b.max_messages,b.last_active,b.created_at, a.id,a.folder,a.name,a.requires_trigger,a.allowed_tools,a.allowed_paths,a.allowed_work_dirs,a.core_prompt,a.model_id,a.created_at,a.updated_at, ch.id,ch.platform_type,ch.name,ch.credentials_json,ch.connection_state,ch.created_at,ch.updated_at FROM bindings b JOIN agents a ON b.agent_id=a.id JOIN channels ch ON b.channel_id=ch.id WHERE b.channel_id=?1 ORDER BY b.id")?;
            let rows: Vec<_> = stmt.query_map(params![channel_id], |r| Ok(row_to_binding_with_relations(r)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect::<Result<Vec<_>>>()
        })
    }

    pub fn delete_binding(&self, id: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM bindings WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    pub fn count_bindings_for_channel(&self, channel_id: i64) -> Result<i64> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM bindings WHERE channel_id=?1",
                params![channel_id],
                |r| r.get::<_, i64>(0),
            )?)
        })
    }

    pub fn update_binding(
        &self,
        id: i64,
        jid: Option<&str>,
        bot_token_override: Option<&str>,
        max_messages: Option<u32>,
        agent_id: Option<i64>,
    ) -> Result<()> {
        self.with_conn(|c| {
            if let Some(j) = jid {
                c.execute("UPDATE bindings SET jid=?1 WHERE id=?2", params![j, id])?;
            }
            if let Some(tok) = bot_token_override {
                c.execute(
                    "UPDATE bindings SET bot_token_override=?1 WHERE id=?2",
                    params![tok, id],
                )?;
            }
            if let Some(mm) = max_messages {
                c.execute(
                    "UPDATE bindings SET max_messages=?1 WHERE id=?2",
                    params![mm, id],
                )?;
            }
            if let Some(aid) = agent_id {
                c.execute(
                    "UPDATE bindings SET agent_id=?1 WHERE id=?2",
                    params![aid, id],
                )?;
            }
            Ok(())
        })
    }

    pub fn touch_binding_active(&self, jid: &str, timestamp: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE bindings SET last_active=?1 WHERE jid=?2",
                params![timestamp, jid],
            )?;
            Ok(())
        })
    }
}

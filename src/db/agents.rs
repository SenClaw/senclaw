use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use crate::types::Agent;

use super::helpers::json_or_null_owned;
use super::rows::row_to_agent;

impl super::Db {
    // ============================================================
    // Agents
    // ============================================================

    pub fn insert_agent(
        &self,
        folder: &str,
        name: &str,
        requires_trigger: bool,
        allowed_tools: Option<&Vec<String>>,
        allowed_work_dirs: Option<&Vec<String>>,
        core_prompt: &str,
        model_id: Option<&str>,
        now: &str,
    ) -> Result<i64> {
        self.with_conn(|c| {
            c.execute("INSERT INTO agents (folder,name,requires_trigger,allowed_tools,allowed_work_dirs,core_prompt,model_id,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8)", params![folder,name,requires_trigger as i64,json_or_null_owned(allowed_tools)?,json_or_null_owned(allowed_work_dirs)?,core_prompt,model_id,now])?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn get_agent(&self, id: i64) -> Result<Option<Agent>> {
        self.with_conn(|c| {
            c.query_row("SELECT * FROM agents WHERE id = ?1", params![id], |r| {
                Ok(row_to_agent(r))
            })
            .optional()?
            .transpose()
        })
    }

    pub fn get_agent_by_folder(&self, folder: &str) -> Result<Option<Agent>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT * FROM agents WHERE folder = ?1",
                params![folder],
                |r| Ok(row_to_agent(r)),
            )
            .optional()?
            .transpose()
        })
    }

    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT * FROM agents ORDER BY id")?;
            let rows: Vec<_> = stmt
                .query_map([], |r| Ok(row_to_agent(r)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect::<Result<Vec<_>>>()
        })
    }

    pub fn delete_agent(&self, id: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM agents WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    pub fn update_agent(
        &self,
        id: i64,
        name: Option<&str>,
        requires_trigger: Option<bool>,
        allowed_tools: Option<&Vec<String>>,
        allowed_work_dirs: Option<&Vec<String>>,
        core_prompt: Option<&str>,
        clear_model_id: bool,
        model_id: Option<&str>,
        now: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            if let Some(n) = name {
                tx.execute(
                    "UPDATE agents SET name=?1,updated_at=?2 WHERE id=?3",
                    rusqlite::params![n, now, id],
                )?;
            }
            if let Some(rt) = requires_trigger {
                tx.execute(
                    "UPDATE agents SET requires_trigger=?1,updated_at=?2 WHERE id=?3",
                    rusqlite::params![rt as i64, now, id],
                )?;
            }
            if let Some(tools) = allowed_tools {
                tx.execute(
                    "UPDATE agents SET allowed_tools=?1,updated_at=?2 WHERE id=?3",
                    rusqlite::params![json_or_null_owned(Some(tools))?, now, id],
                )?;
            }
            if let Some(dirs) = allowed_work_dirs {
                tx.execute(
                    "UPDATE agents SET allowed_work_dirs=?1,updated_at=?2 WHERE id=?3",
                    rusqlite::params![json_or_null_owned(Some(dirs))?, now, id],
                )?;
            }
            if let Some(cp) = core_prompt {
                tx.execute(
                    "UPDATE agents SET core_prompt=?1,updated_at=?2 WHERE id=?3",
                    rusqlite::params![cp, now, id],
                )?;
            }
            if clear_model_id {
                tx.execute(
                    "UPDATE agents SET model_id=NULL,updated_at=?1 WHERE id=?2",
                    rusqlite::params![now, id],
                )?;
            } else if let Some(m) = model_id {
                tx.execute(
                    "UPDATE agents SET model_id=?1,updated_at=?2 WHERE id=?3",
                    rusqlite::params![m, now, id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }
}

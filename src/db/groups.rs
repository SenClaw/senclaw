use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use crate::types::GroupBinding;

use super::helpers::json_or_null;
use super::rows::row_to_group;

impl super::Db {
    // ============================================================
    // Groups
    // ============================================================

    pub fn upsert_group(&self, g: &GroupBinding) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                r#"
                INSERT INTO groups
                  (jid, folder, name, channel, group_type, requires_trigger,
                   allowed_tools, allowed_paths, allowed_work_dirs,
                   bot_token, max_messages, llm_config_id, last_active, added_at,
                   project_id)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
                ON CONFLICT(jid) DO UPDATE SET
                  folder            = excluded.folder,
                  name              = excluded.name,
                  channel           = excluded.channel,
                  group_type        = excluded.group_type,
                  requires_trigger  = excluded.requires_trigger,
                  allowed_tools     = excluded.allowed_tools,
                  allowed_paths     = excluded.allowed_paths,
                  allowed_work_dirs = excluded.allowed_work_dirs,
                  bot_token         = excluded.bot_token,
                  max_messages      = excluded.max_messages,
                  llm_config_id     = excluded.llm_config_id,
                  last_active       = excluded.last_active,
                  -- A session's project is fixed when it is created: a caller
                  -- that builds its binding without one must not detach it.
                  -- Only deleting the project clears it (`unlink_project_groups`).
                  project_id        = COALESCE(excluded.project_id, groups.project_id)
                "#,
                params![
                    g.jid,
                    g.folder,
                    g.name,
                    g.channel,
                    g.group_type,
                    g.requires_trigger as i64,
                    json_or_null(&g.allowed_tools)?,
                    json_or_null(&g.allowed_paths)?,
                    json_or_null(&g.allowed_work_dirs)?,
                    g.bot_token,
                    g.max_messages,
                    g.llm_config_id,
                    g.last_active,
                    g.added_at,
                    g.project_id,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_group(&self, jid: &str) -> Result<Option<GroupBinding>> {
        self.with_conn(|c| {
            let row = c
                .query_row("SELECT * FROM groups WHERE jid = ?1", params![jid], |r| {
                    Ok(row_to_group(r))
                })
                .optional()?;
            row.transpose()
        })
    }

    pub fn list_groups(&self) -> Result<Vec<GroupBinding>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT * FROM groups ORDER BY added_at")?;
            let rows = stmt
                .query_map([], |r| Ok(row_to_group(r)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect::<Result<Vec<_>>>()
        })
    }

    /// The sessions created in a project, oldest first.
    pub fn list_groups_in_project(&self, project_id: &str) -> Result<Vec<GroupBinding>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM groups WHERE project_id = ?1 ORDER BY added_at, jid",
            )?;
            let rows = stmt
                .query_map(params![project_id], |r| Ok(row_to_group(r)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect::<Result<Vec<_>>>()
        })
    }

    /// How many sessions each project has (projects with none are absent).
    pub fn count_groups_per_project(&self) -> Result<std::collections::HashMap<String, u32>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT project_id, COUNT(*) FROM groups WHERE project_id IS NOT NULL GROUP BY project_id",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)))?
                .collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?;
            Ok(rows)
        })
    }

    pub fn delete_group(&self, jid: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM groups WHERE jid = ?1", params![jid])?;
            Ok(())
        })
    }

    pub fn delete_group_by_folder(&self, folder: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM groups WHERE folder = ?1", params![folder])?;
            Ok(())
        })
    }

    pub fn rename_group_jid(&self, old_jid: &str, new_jid: &str) -> Result<Option<GroupBinding>> {
        self.with_conn_mut(|c| {
            let existing: Option<GroupBinding> = c
                .query_row(
                    "SELECT * FROM groups WHERE jid = ?1",
                    params![old_jid],
                    |r| Ok(row_to_group(r)),
                )
                .optional()?
                .transpose()?;
            let Some(mut binding) = existing else {
                return Ok(None);
            };
            binding.jid = new_jid.to_owned();

            // `approved_tools` is not part of GroupBinding — carry it across
            // the delete/insert by hand so "always allow" choices survive.
            let approved_tools: Option<String> = c
                .query_row(
                    "SELECT approved_tools FROM groups WHERE jid = ?1",
                    params![old_jid],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();

            let tx = c.transaction()?;
            tx.execute("DELETE FROM groups WHERE jid = ?1", params![old_jid])?;
            tx.execute(
                r#"
                INSERT INTO groups
                  (jid, folder, name, channel, requires_trigger,
                   allowed_tools, approved_tools, allowed_paths, allowed_work_dirs,
                   bot_token, max_messages, llm_config_id, last_active, added_at,
                   project_id)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
                "#,
                params![
                    binding.jid,
                    binding.folder,
                    binding.name,
                    binding.channel,
                    binding.requires_trigger as i64,
                    json_or_null(&binding.allowed_tools)?,
                    approved_tools,
                    json_or_null(&binding.allowed_paths)?,
                    json_or_null(&binding.allowed_work_dirs)?,
                    binding.bot_token,
                    binding.max_messages,
                    binding.llm_config_id,
                    binding.last_active,
                    binding.added_at,
                    binding.project_id,
                ],
            )?;
            tx.commit()?;
            Ok(Some(binding))
        })
    }

    /// Append a single tool name to a group's `approved_tools` JSON array —
    /// the permission prompt's persisted "always allow" list. This is
    /// deliberately a separate column from `allowed_tools`: that one is the
    /// user-configured use_tools whitelist, and appending approvals to it
    /// used to shrink the whole tool roster to just the approved tools.
    /// No-op if the tool is already in the list or the group does not exist.
    pub fn append_group_approved_tool(&self, jid: &str, tool: &str) -> Result<()> {
        self.with_conn(|c| {
            let raw: Option<String> = c
                .query_row(
                    "SELECT approved_tools FROM groups WHERE jid = ?1",
                    params![jid],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();

            let mut tools: Vec<String> = match raw.as_deref() {
                Some(s) if !s.is_empty() && s != "null" => {
                    serde_json::from_str(s).unwrap_or_default()
                }
                _ => Vec::new(),
            };

            let key = tool.to_string();
            if !tools.contains(&key) {
                tools.push(key);
                let json = serde_json::to_string(&tools)?;
                c.execute(
                    "UPDATE groups SET approved_tools = ?1 WHERE jid = ?2",
                    params![json, jid],
                )?;
            }
            Ok(())
        })
    }

    /// Read a group's persisted "always allow" tool list (`approved_tools`).
    /// Missing group or empty column both yield an empty list.
    pub fn get_group_approved_tools(&self, jid: &str) -> Result<Vec<String>> {
        self.with_conn(|c| {
            let raw: Option<String> = c
                .query_row(
                    "SELECT approved_tools FROM groups WHERE jid = ?1",
                    params![jid],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            Ok(match raw.as_deref() {
                Some(s) if !s.is_empty() && s != "null" => {
                    serde_json::from_str(s).unwrap_or_default()
                }
                _ => Vec::new(),
            })
        })
    }

    pub fn touch_group_active(&self, jid: &str, timestamp: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE groups SET last_active = ?1 WHERE jid = ?2",
                params![timestamp, jid],
            )?;
            Ok(())
        })
    }
}

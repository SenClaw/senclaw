//! Agent lifecycle management.
//! Agents represent AI personas with their own folder, tools, and permissions.

use std::sync::Mutex;

use anyhow::Result;

use crate::config::Config;
use crate::db::Db;
use crate::gateway::group_manager::{ensure_agent_dirs, write_soul_md, GroupManager};
use crate::types::{Agent, GroupBinding};

pub struct AgentManager {
    on_changed: Mutex<Option<Box<dyn Fn() + Send + 'static>>>,
}

impl AgentManager {
    pub fn new() -> Self {
        Self {
            on_changed: Mutex::new(None),
        }
    }

    pub fn set_on_changed(&self, cb: Box<dyn Fn() + Send + 'static>) {
        if let Ok(mut guard) = self.on_changed.lock() {
            *guard = Some(cb);
        }
    }

    fn fire_changed(&self) {
        if let Ok(guard) = self.on_changed.lock() {
            if let Some(ref cb) = *guard {
                cb();
            }
        }
    }

    pub fn create(
        &self,
        db: &Db,
        config: &Config,
        group_manager: &GroupManager,
        folder: &str,
        name: &str,
        requires_trigger: bool,
        allowed_tools: Option<&Vec<String>>,
        allowed_work_dirs: Option<&Vec<String>>,
        core_prompt: &str,
        model_id: Option<&str>,
        now: &str,
    ) -> Result<Agent> {
        // Ensure agent directory + SOUL.md + MEMORY.md + workspace
        ensure_agent_dirs(config, folder, name);
        // Write user's core_prompt to SOUL.md (overrides the default template)
        write_soul_md(config, folder, name, core_prompt);
        spawn_soul_ingest(config.paths.agents_dir.clone(), folder.to_string());

        let id = db.insert_agent(
            folder,
            name,
            requires_trigger,
            allowed_tools,
            allowed_work_dirs,
            core_prompt,
            model_id,
            now,
        )?;

        // Auto-create a default web-only group for the new agent
        let default_group = GroupBinding {
            jid: format!("web:{folder}"),
            folder: folder.to_string(),
            name: name.to_string(),
            channel: String::new(),
            group_type: "chat".to_string(),
            requires_trigger,
            allowed_tools: allowed_tools.cloned(),
            allowed_paths: None,
            allowed_work_dirs: allowed_work_dirs.cloned(),
            bot_token: None,
            max_messages: None,
            llm_config_id: None,
            last_active: None,
            added_at: now.to_string(),
        };
        group_manager.register(db, config, &default_group);

        self.fire_changed();
        db.get_agent(id)?
            .ok_or_else(|| anyhow::anyhow!("Agent {id} not found after insert"))
    }

    pub fn get(&self, db: &Db, id: i64) -> Result<Option<Agent>> {
        db.get_agent(id)
    }

    pub fn get_by_folder(&self, db: &Db, folder: &str) -> Result<Option<Agent>> {
        db.get_agent_by_folder(folder)
    }

    pub fn list(&self, db: &Db) -> Result<Vec<Agent>> {
        db.list_agents()
    }

    pub fn delete(&self, db: &Db, id: i64) -> Result<()> {
        // Note: does NOT delete agent directory on disk (data safety).
        db.delete_agent(id)?;
        self.fire_changed();
        Ok(())
    }

    pub fn update(
        &self,
        db: &Db,
        config: &Config,
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
        db.update_agent(
            id,
            name,
            requires_trigger,
            allowed_tools,
            allowed_work_dirs,
            core_prompt,
            clear_model_id,
            model_id,
            now,
        )?;

        // Sync SOUL.md when core_prompt changes
        if let Some(cp) = core_prompt {
            if let Ok(Some(agent)) = db.get_agent(id) {
                write_soul_md(config, &agent.folder, &agent.name, cp);
                spawn_soul_ingest(config.paths.agents_dir.clone(), agent.folder.clone());
            }
        }

        self.fire_changed();
        Ok(())
    }
}

impl Default for AgentManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Re-cognify an agent's SOUL.md into the cognitive graph after a write.
///
/// Fire-and-forget — runs on the tokio runtime so the HTTP handler that
/// triggered the write returns immediately. No-ops silently when the
/// cognitive system is dormant (no embedding provider). Re-runs always
/// hit content-hash dedupe in the cognify pipeline, so frequent writes
/// only strengthen edges via Hebbian instead of bloating the graph.
pub fn spawn_soul_ingest(agents_dir: std::path::PathBuf, folder: String) {
    tokio::spawn(async move {
        let Some(sys) = crate::memory::cognitive::try_get_instance() else {
            return; // cognitive dormant — silent skip
        };
        match crate::memory::cognitive::ingest_soul_from_disk(&sys, &agents_dir, &folder).await {
            Ok(Some(report)) => {
                tracing::info!(
                    folder = %folder,
                    edges_added = report.edges_added,
                    edges_strengthened = report.edges_strengthened,
                    llm_skipped = report.llm_skipped,
                    "[soul] re-ingested SOUL.md after write"
                );
            }
            Ok(None) => { /* file missing — would mean write_soul_md just failed */ }
            Err(e) => tracing::warn!(folder = %folder, error = %e, "[soul] re-ingest failed"),
        }
    });
}

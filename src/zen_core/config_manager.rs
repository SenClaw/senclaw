//! ConfigManager — project-scoped configuration with persistence.
//!
//! Port of TS `manager/ConfManager.ts`.
//!
//! Stores per-working-directory project config in
//! `~/.senclaw/project-config.json` keyed by absolute path.
//! Keeps at most `PROJECT_LENGTH_LIMIT` projects (evicts by `last_edit_time`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ============================================================================
// Constants
// ============================================================================

const PROJECT_LENGTH_LIMIT: usize = 20;
const PROJECT_HISTORY_LENGTH_LIMIT: usize = 30;

// ============================================================================
// Types
// ============================================================================

/// Per-project (per-working-dir) configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Tool names the user has explicitly allowed for this project.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Recent user prompts (newest first).
    #[serde(default)]
    pub history: Vec<String>,
    /// Custom project rules / instructions.
    #[serde(default)]
    pub rules: Vec<String>,
    /// ISO-8601 local time of last edit.
    pub last_edit_time: String,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            allowed_tools: Vec::new(),
            history: Vec::new(),
            rules: Vec::new(),
            last_edit_time: current_time_str(),
        }
    }
}

/// Serialised form of the file: a map from working-dir path → ProjectConfig.
type GlobalProjectConfig = HashMap<String, ProjectConfig>;

/// Keys in `ZenCoreOptions` / `SemaCoreConfig` that callers are allowed to
/// update via `update_core_conf_by_key` without a full re-init.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatableCoreKey {
    Stream,
    Thinking,
    SystemPrompt,
    CustomRules,
    SkipFileEditPermission,
    SkipBashExecPermission,
    SkipSkillPermission,
    SkipMcpToolPermission,
    EnableLlmCache,
}

impl UpdatableCoreKey {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "stream" => Some(Self::Stream),
            "thinking" => Some(Self::Thinking),
            "system_prompt" | "systemPrompt" => Some(Self::SystemPrompt),
            "custom_rules" | "customRules" => Some(Self::CustomRules),
            "skip_file_edit_permission" | "skipFileEditPermission" => {
                Some(Self::SkipFileEditPermission)
            }
            "skip_bash_exec_permission" | "skipBashExecPermission" => {
                Some(Self::SkipBashExecPermission)
            }
            "skip_skill_permission" | "skipSkillPermission" => Some(Self::SkipSkillPermission),
            "skip_mcp_tool_permission" | "skipMCPToolPermission" => {
                Some(Self::SkipMcpToolPermission)
            }
            "enable_llm_cache" | "enableLLMCache" => Some(Self::EnableLlmCache),
            _ => None,
        }
    }
}

// ============================================================================
// ConfigManager
// ============================================================================

pub struct ConfigManager {
    global_config: Mutex<GlobalProjectConfig>,
    config_path: PathBuf,
}

impl ConfigManager {
    /// Create a new instance backed by `config_path`.
    pub fn new(config_path: PathBuf) -> Self {
        let global_config = load_global_config(&config_path).unwrap_or_default();
        Self {
            global_config: Mutex::new(global_config),
            config_path,
        }
    }

    // ------------------------------------------------------------------ read

    /// Return the stored `ProjectConfig` for `working_dir`, or `None`.
    pub fn get_project_config(&self, working_dir: &str) -> Option<ProjectConfig> {
        self.global_config.lock().unwrap().get(working_dir).cloned()
    }

    // ----------------------------------------------------------------- write

    /// Register a working directory. Creates a default `ProjectConfig` if
    /// none exists yet. Returns `true` when a new project was created.
    pub fn register_project(&self, working_dir: &str) -> bool {
        let mut cfg = self.global_config.lock().unwrap();
        if cfg.contains_key(working_dir) {
            return false;
        }
        cfg.insert(working_dir.to_owned(), ProjectConfig::default());
        self.cleanup_old_projects_locked(&mut cfg);
        drop(cfg);
        if let Err(e) = self.save() {
            warn!("[ConfigManager] save failed: {e}");
        }
        true
    }

    /// Upsert `ProjectConfig` fields for `working_dir`.
    pub fn set_project_config(&self, working_dir: &str, patch: ProjectConfigPatch) -> Result<()> {
        let mut cfg = self.global_config.lock().unwrap();
        let entry = cfg.entry(working_dir.to_owned()).or_default();

        if let Some(tools) = patch.allowed_tools {
            entry.allowed_tools = tools;
        }
        if let Some(mut hist) = patch.history {
            hist.truncate(PROJECT_HISTORY_LENGTH_LIMIT);
            entry.history = hist;
        }
        if let Some(rules) = patch.rules {
            entry.rules = rules;
        }
        entry.last_edit_time = current_time_str();
        drop(cfg);
        self.save()
    }

    /// Prepend a user prompt to the history for `working_dir`.
    pub fn save_user_input_to_history(&self, working_dir: &str, input: &str) {
        let result = {
            let mut cfg = self.global_config.lock().unwrap();
            let entry = cfg.entry(working_dir.to_owned()).or_default();
            entry.history.insert(0, input.to_owned());
            if entry.history.len() > PROJECT_HISTORY_LENGTH_LIMIT {
                entry.history.truncate(PROJECT_HISTORY_LENGTH_LIMIT);
            }
            entry.last_edit_time = current_time_str();
            drop(cfg);
            self.save()
        };
        if let Err(e) = result {
            warn!("[ConfigManager] save_user_input_to_history failed: {e}");
        }
    }

    // ---------------------------------------------------------------- persist

    /// Persist `global_config` to disk.
    pub fn save(&self) -> Result<()> {
        let cfg = self.global_config.lock().unwrap();
        let dir = self.config_path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("create config dir {:?}", dir))?;
        let json = serde_json::to_string_pretty(&*cfg)?;
        std::fs::write(&self.config_path, json)
            .with_context(|| format!("write config {:?}", self.config_path))?;
        info!("[ConfigManager] saved to {:?}", self.config_path);
        Ok(())
    }

    // --------------------------------------------------------------- private

    fn cleanup_old_projects_locked(&self, cfg: &mut GlobalProjectConfig) {
        if cfg.len() <= PROJECT_LENGTH_LIMIT {
            return;
        }
        let mut entries: Vec<(String, ProjectConfig)> = cfg.drain().collect();
        entries.sort_by(|a, b| b.1.last_edit_time.cmp(&a.1.last_edit_time));
        entries.truncate(PROJECT_LENGTH_LIMIT);
        *cfg = entries.into_iter().collect();
    }
}

// ============================================================================
// Patch type (partial update)
// ============================================================================

#[derive(Debug, Default)]
pub struct ProjectConfigPatch {
    pub allowed_tools: Option<Vec<String>>,
    pub history: Option<Vec<String>>,
    pub rules: Option<Vec<String>>,
}

// ============================================================================
// Global singleton
// ============================================================================

static GLOBAL_CONF_MANAGER: OnceLock<Mutex<Option<ConfigManager>>> = OnceLock::new();

fn conf_manager_cell() -> &'static Mutex<Option<ConfigManager>> {
    GLOBAL_CONF_MANAGER.get_or_init(|| Mutex::new(None))
}

/// Get (or lazily create) the global `ConfigManager` singleton.
pub fn get_conf_manager() -> &'static Mutex<Option<ConfigManager>> {
    let cell = conf_manager_cell();
    {
        let mut guard = cell.lock().unwrap();
        if guard.is_none() {
            let path = default_project_config_path();
            *guard = Some(ConfigManager::new(path));
        }
    }
    cell
}

/// Convenience: run a closure with the singleton and return its result.
pub fn with_conf_manager<F, T>(f: F) -> T
where
    F: FnOnce(&ConfigManager) -> T,
{
    let cell = get_conf_manager();
    let guard = cell.lock().unwrap();
    f(guard.as_ref().expect("ConfigManager not initialised"))
}

// ============================================================================
// Helpers
// ============================================================================

fn default_project_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".senclaw")
        .join("project-config.json")
}

fn load_global_config(path: &Path) -> Option<GlobalProjectConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn current_time_str() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn mgr_in(dir: &Path) -> ConfigManager {
        ConfigManager::new(dir.join("project-config.json"))
    }

    #[test]
    fn new_project_created_on_register() {
        let tmp = tempdir().unwrap();
        let mgr = mgr_in(tmp.path());
        let created = mgr.register_project("/workspace/my-project");
        assert!(created);
        assert!(mgr.get_project_config("/workspace/my-project").is_some());
    }

    #[test]
    fn register_idempotent() {
        let tmp = tempdir().unwrap();
        let mgr = mgr_in(tmp.path());
        assert!(mgr.register_project("/workspace/proj"));
        assert!(!mgr.register_project("/workspace/proj")); // second call → false
    }

    #[test]
    fn set_project_config_updates_fields() {
        let tmp = tempdir().unwrap();
        let mgr = mgr_in(tmp.path());
        mgr.register_project("/workspace/proj");
        mgr.set_project_config(
            "/workspace/proj",
            ProjectConfigPatch {
                allowed_tools: Some(vec!["Bash".into(), "Read".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        let cfg = mgr.get_project_config("/workspace/proj").unwrap();
        assert_eq!(cfg.allowed_tools, vec!["Bash", "Read"]);
    }

    #[test]
    fn history_truncated_to_limit() {
        let tmp = tempdir().unwrap();
        let mgr = mgr_in(tmp.path());
        mgr.register_project("/w");
        for i in 0..=40 {
            mgr.save_user_input_to_history("/w", &format!("prompt {i}"));
        }
        let cfg = mgr.get_project_config("/w").unwrap();
        assert!(cfg.history.len() <= PROJECT_HISTORY_LENGTH_LIMIT);
        // newest entry is first
        assert_eq!(cfg.history[0], "prompt 40");
    }

    #[test]
    fn persisted_and_reloaded() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("project-config.json");

        {
            let mgr = ConfigManager::new(path.clone());
            mgr.register_project("/w/reloaded");
            mgr.set_project_config(
                "/w/reloaded",
                ProjectConfigPatch {
                    rules: Some(vec!["rule-1".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        // Reload from disk
        let mgr2 = ConfigManager::new(path);
        let cfg = mgr2.get_project_config("/w/reloaded").unwrap();
        assert_eq!(cfg.rules, vec!["rule-1"]);
    }

    #[test]
    fn old_projects_evicted_over_limit() {
        let tmp = tempdir().unwrap();
        let mgr = mgr_in(tmp.path());
        for i in 0..25 {
            mgr.register_project(&format!("/w/{i}"));
        }
        let cfg = mgr.global_config.lock().unwrap();
        assert!(cfg.len() <= PROJECT_LENGTH_LIMIT);
    }
}

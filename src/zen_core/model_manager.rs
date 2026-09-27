//! ModelManager — CRUD for LLM model profiles with file persistence.
//!
//! Port of TS `manager/ModelManager.ts`.
//!
//! Config is stored at `~/.senclaw/models.json` as:
//! ```json
//! {
//!   "modelProfiles": [ { "name": "...", "provider": "...", ... } ],
//!   "modelPointers": { "main": "gpt4o", "quick": "gpt4o-mini" }
//! }
//! ```

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::zen_core::ModelProfile;

// ============================================================================
// Types
// ============================================================================

/// Pointer key: which slot a profile is assigned to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelPointerType {
    Main,
    Quick,
}

impl ModelPointerType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelPointerType::Main => "main",
            ModelPointerType::Quick => "quick",
        }
    }
}

/// Persisted model configuration file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfiguration {
    #[serde(default)]
    pub model_profiles: Vec<ModelProfile>,
    #[serde(default)]
    pub model_pointers: ModelPointers,
}

impl Default for ModelConfiguration {
    fn default() -> Self {
        Self {
            model_profiles: Vec::new(),
            model_pointers: ModelPointers::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelPointers {
    #[serde(default)]
    pub main: String,
    #[serde(default)]
    pub quick: String,
}

/// Return value for mutating operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUpdateData {
    pub model_name: String,
    pub model_list: Vec<String>,
    pub task_config: TaskConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfig {
    pub main: String,
    pub quick: String,
}

/// Input when adding a model (user-visible fields before conversion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAddInput {
    pub name: String,
    pub provider: String,
    pub model_name: String,
    pub base_url: String,
    pub api_key: String,
    pub max_tokens: u32,
    pub context_length: u32,
    pub adapt: Option<String>,
}

// ============================================================================
// ModelManager
// ============================================================================

pub struct ModelManager {
    config: Mutex<ModelConfiguration>,
    config_path: PathBuf,
}

impl ModelManager {
    /// Create a new instance backed by `config_path`.
    pub fn new(config_path: PathBuf) -> Self {
        let config = load_model_config(&config_path).unwrap_or_default();
        Self {
            config: Mutex::new(config),
            config_path,
        }
    }

    // ------------------------------------------------------------------ read

    /// Return the profile assigned to `pointer` (main / quick), or `None`.
    pub fn get_model(&self, pointer: ModelPointerType) -> Option<ModelProfile> {
        let cfg = self.config.lock().unwrap();
        let id = match pointer {
            ModelPointerType::Main => &cfg.model_pointers.main,
            ModelPointerType::Quick => &cfg.model_pointers.quick,
        };
        find_profile(id, &cfg.model_profiles)
    }

    /// Return the model_name string for `pointer`, or `None`.
    pub fn get_model_name(&self, pointer: ModelPointerType) -> Option<String> {
        self.get_model(pointer).map(|p| p.model_name)
    }

    /// Return summary data (no credentials).
    pub fn get_model_data(&self) -> ModelUpdateData {
        let cfg = self.config.lock().unwrap();
        build_update_data(&cfg)
    }

    /// Return all profiles (cloned).
    pub fn list_profiles(&self) -> Vec<ModelProfile> {
        self.config.lock().unwrap().model_profiles.clone()
    }

    // ----------------------------------------------------------------- write

    /// Add or update a model profile.
    /// Does **not** do API validation — callers should validate beforehand if
    /// needed (the TS version had an optional `skipValidation` parameter;
    /// here validation is always the caller's responsibility).
    pub fn add_model(&self, input: ModelAddInput) -> Result<ModelUpdateData> {
        let profile = model_profile_from_input(input);
        let mut cfg = self.config.lock().unwrap();

        match cfg
            .model_profiles
            .iter()
            .position(|p| p.name == profile.name)
        {
            Some(idx) => {
                info!(
                    "[ModelManager] updating existing profile '{}'",
                    profile.name
                );
                cfg.model_profiles[idx] = profile;
            }
            None => {
                info!("[ModelManager] adding new profile '{}'", profile.name);
                cfg.model_profiles.push(profile.clone());
                // First model → set both pointers
                if cfg.model_profiles.len() == 1 {
                    cfg.model_pointers.main = profile.name.clone();
                    cfg.model_pointers.quick = profile.name;
                }
            }
        }

        let data = build_update_data(&cfg);
        drop(cfg);
        self.save()?;
        Ok(data)
    }

    /// Delete a model by name.
    /// Returns an error if the model is referenced by any pointer.
    pub fn delete_model(&self, name: &str) -> Result<ModelUpdateData> {
        let mut cfg = self.config.lock().unwrap();

        let idx = cfg
            .model_profiles
            .iter()
            .position(|p| p.name == name)
            .with_context(|| format!("model '{name}' not found"))?;

        // Check pointer references
        let mut used_in: Vec<&str> = Vec::new();
        if cfg.model_pointers.main == name {
            used_in.push("main");
        }
        if cfg.model_pointers.quick == name {
            used_in.push("quick");
        }
        if !used_in.is_empty() {
            bail!(
                "cannot delete model '{name}': referenced by pointer(s): {}",
                used_in.join(", ")
            );
        }

        cfg.model_profiles.remove(idx);
        info!("[ModelManager] deleted profile '{name}'");
        let data = build_update_data(&cfg);
        drop(cfg);
        self.save()?;
        Ok(data)
    }

    /// Point `main` at `name`.
    pub fn switch_current_model(&self, name: &str) -> Result<ModelUpdateData> {
        let mut cfg = self.config.lock().unwrap();
        ensure_profile_exists(name, &cfg.model_profiles)?;
        cfg.model_pointers.main = name.to_owned();
        if cfg.model_pointers.quick.is_empty() {
            cfg.model_pointers.quick = name.to_owned();
        }
        info!("[ModelManager] main → '{name}'");
        let data = build_update_data(&cfg);
        drop(cfg);
        self.save()?;
        Ok(data)
    }

    /// Update both `main` and `quick` pointers.
    pub fn apply_task_model_config(&self, task: TaskConfig) -> Result<ModelUpdateData> {
        let mut cfg = self.config.lock().unwrap();
        ensure_profile_exists(&task.main, &cfg.model_profiles)
            .with_context(|| format!("main model '{}'", task.main))?;
        ensure_profile_exists(&task.quick, &cfg.model_profiles)
            .with_context(|| format!("quick model '{}'", task.quick))?;
        cfg.model_pointers.main = task.main.clone();
        cfg.model_pointers.quick = task.quick.clone();
        info!(
            "[ModelManager] main → '{}', quick → '{}'",
            task.main, task.quick
        );
        let data = build_update_data(&cfg);
        drop(cfg);
        self.save()?;
        Ok(data)
    }

    // ---------------------------------------------------------------- persist

    pub fn save(&self) -> Result<()> {
        let cfg = self.config.lock().unwrap();
        let dir = self.config_path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("create config dir {dir:?}"))?;
        let json = serde_json::to_string_pretty(&*cfg)?;
        std::fs::write(&self.config_path, json)
            .with_context(|| format!("write model config {:?}", self.config_path))?;
        info!("[ModelManager] saved to {:?}", self.config_path);
        Ok(())
    }
}

// ============================================================================
// Global singleton
// ============================================================================

static GLOBAL_MODEL_MANAGER: OnceLock<Mutex<Option<ModelManager>>> = OnceLock::new();

fn model_manager_cell() -> &'static Mutex<Option<ModelManager>> {
    GLOBAL_MODEL_MANAGER.get_or_init(|| Mutex::new(None))
}

/// Get (or lazily create) the global `ModelManager` singleton.
pub fn get_model_manager() -> &'static Mutex<Option<ModelManager>> {
    let cell = model_manager_cell();
    {
        let mut guard = cell.lock().unwrap();
        if guard.is_none() {
            let path = default_model_config_path();
            *guard = Some(ModelManager::new(path));
        }
    }
    cell
}

/// Convenience: run a closure with the singleton.
pub fn with_model_manager<F, T>(f: F) -> T
where
    F: FnOnce(&ModelManager) -> T,
{
    let cell = get_model_manager();
    let guard = cell.lock().unwrap();
    f(guard.as_ref().expect("ModelManager not initialised"))
}

// ============================================================================
// Helpers
// ============================================================================

fn default_model_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".senclaw")
        .join("models.json")
}

fn load_model_config(path: &Path) -> Option<ModelConfiguration> {
    let content = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<ModelConfiguration>(&content) {
        Ok(c) => Some(c),
        Err(e) => {
            warn!("[ModelManager] failed to parse {:?}: {e}", path);
            None
        }
    }
}

fn find_profile(name: &str, profiles: &[ModelProfile]) -> Option<ModelProfile> {
    if name.is_empty() {
        return None;
    }
    profiles.iter().find(|p| p.name == name).cloned()
}

fn ensure_profile_exists(name: &str, profiles: &[ModelProfile]) -> Result<()> {
    if find_profile(name, profiles).is_none() {
        bail!("model '{name}' not found");
    }
    Ok(())
}

fn build_update_data(cfg: &ModelConfiguration) -> ModelUpdateData {
    ModelUpdateData {
        model_name: cfg.model_pointers.main.clone(),
        model_list: cfg.model_profiles.iter().map(|p| p.name.clone()).collect(),
        task_config: TaskConfig {
            main: cfg.model_pointers.main.clone(),
            quick: cfg.model_pointers.quick.clone(),
        },
    }
}

fn model_profile_from_input(input: ModelAddInput) -> ModelProfile {
    ModelProfile {
        name: input.name,
        provider: input.provider,
        model_name: input.model_name,
        base_url: input.base_url,
        api_key: input.api_key,
        max_tokens: input.max_tokens,
        context_length: input.context_length,
        adapt: input.adapt,
        vision: None,
        ..Default::default()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_input(name: &str) -> ModelAddInput {
        ModelAddInput {
            name: name.to_string(),
            provider: "openai".into(),
            model_name: "gpt-4o".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: "sk-test".into(),
            max_tokens: 4096,
            context_length: 128000,
            adapt: Some("openai".into()),
        }
    }

    fn mgr_in(dir: &Path) -> ModelManager {
        ModelManager::new(dir.join("models.json"))
    }

    #[test]
    fn add_first_model_sets_pointers() {
        let tmp = tempdir().unwrap();
        let mgr = mgr_in(tmp.path());
        let data = mgr.add_model(sample_input("gpt4o")).unwrap();
        assert_eq!(data.model_name, "gpt4o");
        assert_eq!(data.task_config.main, "gpt4o");
        assert_eq!(data.task_config.quick, "gpt4o");
    }

    #[test]
    fn add_second_model_does_not_change_pointers() {
        let tmp = tempdir().unwrap();
        let mgr = mgr_in(tmp.path());
        mgr.add_model(sample_input("model-a")).unwrap();
        mgr.add_model(sample_input("model-b")).unwrap();
        let data = mgr.get_model_data();
        assert_eq!(data.task_config.main, "model-a");
    }

    #[test]
    fn switch_current_model() {
        let tmp = tempdir().unwrap();
        let mgr = mgr_in(tmp.path());
        mgr.add_model(sample_input("model-a")).unwrap();
        mgr.add_model(sample_input("model-b")).unwrap();
        let data = mgr.switch_current_model("model-b").unwrap();
        assert_eq!(data.model_name, "model-b");
    }

    #[test]
    fn delete_unreferenced_model() {
        let tmp = tempdir().unwrap();
        let mgr = mgr_in(tmp.path());
        mgr.add_model(sample_input("model-a")).unwrap();
        mgr.add_model(sample_input("model-b")).unwrap();
        // Move both pointers off model-a before deleting it
        mgr.apply_task_model_config(TaskConfig {
            main: "model-b".into(),
            quick: "model-b".into(),
        })
        .unwrap();
        let data = mgr.delete_model("model-a").unwrap();
        assert!(!data.model_list.contains(&"model-a".to_string()));
    }

    #[test]
    fn delete_referenced_model_fails() {
        let tmp = tempdir().unwrap();
        let mgr = mgr_in(tmp.path());
        mgr.add_model(sample_input("model-a")).unwrap();
        let err = mgr.delete_model("model-a").unwrap_err();
        assert!(err.to_string().contains("referenced by pointer"));
    }

    #[test]
    fn persisted_and_reloaded() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("models.json");
        {
            let mgr = ModelManager::new(path.clone());
            mgr.add_model(sample_input("saved-model")).unwrap();
        }
        let mgr2 = ModelManager::new(path);
        assert_eq!(mgr2.list_profiles().len(), 1);
        assert_eq!(mgr2.list_profiles()[0].name, "saved-model");
    }

    #[test]
    fn apply_task_model_config_updates_both_pointers() {
        let tmp = tempdir().unwrap();
        let mgr = mgr_in(tmp.path());
        mgr.add_model(sample_input("big")).unwrap();
        mgr.add_model(sample_input("small")).unwrap();
        mgr.switch_current_model("big").unwrap();
        let data = mgr
            .apply_task_model_config(TaskConfig {
                main: "big".into(),
                quick: "small".into(),
            })
            .unwrap();
        assert_eq!(data.task_config.quick, "small");
    }
}

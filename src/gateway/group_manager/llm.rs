//! LLM config, thinking config, and admin permissions.

use std::path::Path;

use anyhow::Result;

use super::config::{load_global_config, save_global_config};
use super::types::{
    AdminPermissions, AdminPermissionsSection, DefaultsConfig, EmbeddingConfig, LlmConfig,
    LlmConfigResult,
};

// ===== Admin permissions config =====

pub fn get_admin_permissions_config(config_path: &Path) -> AdminPermissions {
    let cfg = load_global_config(config_path);
    let p = cfg.admin_permissions.unwrap_or_default();
    AdminPermissions {
        skip_main_agent_permissions: p.skip_main_agent_permissions.unwrap_or(false),
        skip_all_agents_permissions: p.skip_all_agents_permissions.unwrap_or(false),
    }
}

pub fn save_admin_permissions_config(config_path: &Path, opts: &AdminPermissions) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.admin_permissions = Some(AdminPermissionsSection {
        skip_main_agent_permissions: Some(opts.skip_main_agent_permissions),
        skip_all_agents_permissions: Some(opts.skip_all_agents_permissions),
    });
    save_global_config(config_path, &cfg)
}

// ===== Thinking config =====

pub fn get_thinking_enabled(config_path: &Path) -> bool {
    load_global_config(config_path)
        .thinking_enabled
        .unwrap_or(true)
}

pub fn save_thinking_enabled(config_path: &Path, enabled: bool) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.thinking_enabled = Some(enabled);
    save_global_config(config_path, &cfg)
}

// ===== Pre-process stage toggles (global, user-set) =====

/// Pre-trigger-skill stage. Default OFF — opt-in deterministic skill force-load.
pub fn get_pre_trigger_skill_enabled(config_path: &Path) -> bool {
    load_global_config(config_path)
        .pre_trigger_skill
        .unwrap_or(false)
}

pub fn save_pre_trigger_skill_enabled(config_path: &Path, enabled: bool) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.pre_trigger_skill = Some(enabled);
    save_global_config(config_path, &cfg)
}

/// Pre-cognitive stage. Default OFF — opt-in cognitive-memory injection.
pub fn get_pre_cognitive_enabled(config_path: &Path) -> bool {
    load_global_config(config_path)
        .pre_cognitive
        .unwrap_or(false)
}

pub fn save_pre_cognitive_enabled(config_path: &Path, enabled: bool) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.pre_cognitive = Some(enabled);
    save_global_config(config_path, &cfg)
}

// ===== After-process stage toggle (global, user-set) =====

/// After-process / context-update stage. Default OFF — opt-in.
///
/// When enabled, after the main agent turn completes the conversation is
/// summarised and context is updated (Claude-Code style) so the agent retains
/// a compact, optimised understanding of the whole dialogue for future turns.
pub fn get_after_process_enabled(config_path: &Path) -> bool {
    load_global_config(config_path)
        .after_process
        .unwrap_or(false)
}

pub fn save_after_process_enabled(config_path: &Path, enabled: bool) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.after_process = Some(enabled);
    save_global_config(config_path, &cfg)
}

// ===== Curated-memory stage toggle (global, user-set) =====

/// Curated-memory stage. Default ON — auto-recall + consolidation after
/// compaction so session facts survive context drops (Claude-Code-style).
/// Users can still turn it off in Settings → Agent behavior.
///
/// When enabled: (a) history dropped by compaction is consolidated into
/// curated `memory/*.md` files, and (b) each request injects relevant curated
/// memories found via hybrid FTS5/vector search.
pub fn get_memory_recall_enabled(config_path: &Path) -> bool {
    load_global_config(config_path)
        .memory_recall
        .unwrap_or(true)
}

pub fn save_memory_recall_enabled(config_path: &Path, enabled: bool) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.memory_recall = Some(enabled);
    save_global_config(config_path, &cfg)
}

// ===== Default flows + widget disable list (global, user-set) =====

/// The `defaults` section (open-link / media / search / note handlers + the
/// per-widget disable list). Absent section → all-`None` config whose
/// `effective_*()` fallbacks reproduce today's behavior exactly.
pub fn get_defaults_config(config_path: &Path) -> DefaultsConfig {
    load_global_config(config_path).defaults.unwrap_or_default()
}

/// Merge-save: only `Some` fields in `patch` replace the stored values, so the
/// UI can update one dropdown without resending the rest. Returns the merged
/// result.
pub fn save_defaults_config(config_path: &Path, patch: &DefaultsConfig) -> Result<DefaultsConfig> {
    let mut cfg = load_global_config(config_path);
    let mut current = cfg.defaults.take().unwrap_or_default();
    if patch.open_link.is_some() {
        current.open_link = patch.open_link.clone();
    }
    if patch.media.is_some() {
        current.media = patch.media.clone();
    }
    if patch.search.is_some() {
        current.search = patch.search.clone();
    }
    if patch.search_engine.is_some() {
        current.search_engine = patch.search_engine.clone();
    }
    if patch.note.is_some() {
        current.note = patch.note.clone();
    }
    if patch.disabled_widgets.is_some() {
        current.disabled_widgets = patch.disabled_widgets.clone();
    }
    cfg.defaults = Some(current.clone());
    save_global_config(config_path, &cfg)?;
    Ok(current)
}

/// Toggle one widget id in `defaults.disabledWidgets`.
pub fn set_widget_disabled(config_path: &Path, widget_id: &str, disabled: bool) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    let mut current = cfg.defaults.take().unwrap_or_default();
    let mut list = current.disabled_widgets.take().unwrap_or_default();
    list.retain(|id| id != widget_id);
    if disabled {
        list.push(widget_id.to_string());
    }
    current.disabled_widgets = Some(list);
    cfg.defaults = Some(current);
    save_global_config(config_path, &cfg)
}

// ===== MCP dispatcher toggle (global, user-set) =====

/// Autonomous MCP dispatcher. Default OFF — opt-in autonomous task execution:
/// when enabled, ready tasks on dispatch sources (the Kanban board) are picked
/// up and run by persona worker agents.
pub fn get_dispatch_enabled(config_path: &Path) -> bool {
    load_global_config(config_path)
        .dispatch_enabled
        .unwrap_or(false)
}

pub fn save_dispatch_enabled(config_path: &Path, enabled: bool) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.dispatch_enabled = Some(enabled);
    save_global_config(config_path, &cfg)
}

// ===== LLM config =====

/// Every model SenClaw can route a turn to: the user's own configs from
/// `config.json`, plus the models served by installed Space Apps.
///
/// The app-provided half is appended **here**, not at the HTTP layer, because
/// this is the one function every model decision goes through — the picker,
/// `ZenEngine::resolve_model_profile_at`, and the vision check that wraps it.
/// Merging further out would put models in the picker that fail with "config
/// not found" as soon as one is selected.
///
/// App configs come last so a user's own config always wins a duplicate id, and
/// so `configs[0]` — the fallback when no active model is set — stays the user's
/// model rather than becoming whichever app happened to install first.
pub fn load_llm_configs(config_path: &Path) -> LlmConfigResult {
    let cfg = load_global_config(config_path);
    let mut configs = cfg.llm_configs.unwrap_or_default();
    for c in crate::apps::llm_provider::configs() {
        if !configs.iter().any(|x| x.id == c.id) {
            configs.push(c);
        }
    }
    // Local models (§5.4): scanned fresh off disk every read, never persisted
    // — the same reason app configs are appended here rather than saved. The
    // root is the one the daemon registered at boot for this config file
    // (`SENCLAW_LOCAL_MODELS_DIR` may point anywhere), else the sibling default.
    let local_models_dir = crate::local_models::root_for(config_path);
    if let Some(dir) = &local_models_dir {
        for c in crate::local_models::rest::llm_configs(dir) {
            if !configs.iter().any(|x| x.id == c.id) {
                configs.push(c);
            }
        }
    }

    let mut active_id = cfg.active_llm_config_id;
    // Migration: `apps/mlx-lm` (a Space App) became the `sen-mlx` runtime.
    // A machine that had it selected keeps working by mapping the old
    // `app:mlx-lm:<model>` id to the matching `local:` model, when one
    // exists — otherwise the stale id is left as-is (it will simply not
    // resolve, same as any other uninstalled provider).
    if let Some(id) = &active_id {
        if let Some(model) = id.strip_prefix("app:mlx-lm:") {
            if let Some(replacement) = configs.iter().find(|c| {
                c.id.starts_with(crate::local_models::ID_PREFIX)
                    && (c.model_name.eq_ignore_ascii_case(model) || c.label.to_ascii_lowercase().contains(&model.to_ascii_lowercase()))
            }) {
                active_id = Some(replacement.id.clone());
            }
        }
    }

    LlmConfigResult {
        configs,
        active_id,
        active_quick_id: cfg.active_quick_llm_config_id,
        active_cognitive_id: cfg.active_cognitive_llm_config_id,
    }
}

/// Persist a user-created config.
///
/// App-provided and local-model configs are refused. Neither is stored in
/// `config.json` — both are rebuilt fresh on every read (from the app
/// registry, or by rescanning `local-models/`) — and writing one here would
/// freeze a copy that outlives the app or the file it pointed at: still in the
/// picker after an uninstall or a delete, no longer updated when the source
/// changes what it serves.
pub fn save_llm_config(config_path: &Path, c: &LlmConfig) -> Result<()> {
    if crate::local_models::is_local_config(&c.id) {
        anyhow::bail!(
            "`{}` is a local model and cannot be edited here; manage it from Settings → Local models",
            c.id
        );
    }
    if crate::apps::llm_provider::is_app_config(&c.id) {
        anyhow::bail!(
            "`{}` belongs to a Space App and cannot be edited here; \
             change it in the app, or uninstall the app to remove it",
            c.id
        );
    }
    let mut cfg = load_global_config(config_path);
    let configs = cfg.llm_configs.get_or_insert_with(Vec::new);
    if let Some(existing) = configs.iter_mut().find(|x| x.id == c.id) {
        *existing = c.clone();
    } else {
        configs.push(c.clone());
    }
    save_global_config(config_path, &cfg)
}

pub fn remove_llm_config(config_path: &Path, id: &str) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    if let Some(ref mut configs) = cfg.llm_configs {
        configs.retain(|x| x.id != id);
    }
    if cfg.active_llm_config_id.as_deref() == Some(id) {
        cfg.active_llm_config_id = None;
    }
    if cfg.active_quick_llm_config_id.as_deref() == Some(id) {
        cfg.active_quick_llm_config_id = None;
    }
    if cfg.active_cognitive_llm_config_id.as_deref() == Some(id) {
        cfg.active_cognitive_llm_config_id = None;
    }
    save_global_config(config_path, &cfg)
}

pub fn set_active_llm_config(config_path: &Path, id: Option<&str>) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.active_llm_config_id = id.map(|s| s.to_string());
    save_global_config(config_path, &cfg)
}

pub fn set_active_quick_llm_config(config_path: &Path, id: Option<&str>) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.active_quick_llm_config_id = id.map(|s| s.to_string());
    save_global_config(config_path, &cfg)
}

pub fn set_active_cognitive_llm_config(config_path: &Path, id: Option<&str>) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.active_cognitive_llm_config_id = id.map(|s| s.to_string());
    save_global_config(config_path, &cfg)
}
// ===== Embedding config =====

pub fn load_embedding_config(config_path: &Path) -> Option<EmbeddingConfig> {
    load_global_config(config_path).embedding_config
}

pub fn load_cognitive_config(config_path: &Path) -> Option<super::types::PersistedCognitiveConfig> {
    load_global_config(config_path).cognitive_config
}

pub fn save_cognitive_config(
    config_path: &Path,
    c: &super::types::PersistedCognitiveConfig,
) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.cognitive_config = Some(c.clone());
    save_global_config(config_path, &cfg)
}

pub fn save_embedding_config(config_path: &Path, c: &EmbeddingConfig) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.embedding_config = Some(c.clone());
    save_global_config(config_path, &cfg)
}

// ===== Legacy engine settings (whisperConfig/ttsConfig/ocrConfig) =====
//
// Whisper, TTS and OCR each own their settings now, in their runtime's own
// `<data_dir>/settings.json` (seeded from these keys on first start —
// `sen_runtime_sdk::legacy::load_or_import`). The daemon never reads or
// writes them again; the fields stay `Option<Value>` on `GlobalConfig` purely
// so a save of an unrelated section (LLM configs, embedding settings, …)
// round-trips them untouched instead of silently dropping them before a
// runtime ever gets to import them (docs/runtime-protocol.md §8).

// ===== Decision (typed-answer) settings =====
//
// `decisionConfig` also holds `backend`/`local`/`online`, which the
// `sen-sysone` runtime now owns as its own opaque sub-keys of the same
// object. Only `.gate`/`.skills` are the daemon's — see
// `crate::decision::settings::DecisionSettings`.

pub fn load_decision_settings(config_path: &Path) -> crate::decision::settings::DecisionSettings {
    let raw = load_global_config(config_path).decision_config.unwrap_or_default();
    crate::decision::settings::DecisionSettings::from_raw(&raw)
}

/// Set `.gate`/`.skills` on `decisionConfig`, leaving `backend`/`local`/
/// `online` exactly as they were.
pub fn save_decision_settings(config_path: &Path, s: &crate::decision::settings::DecisionSettings) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    let mut raw = cfg.decision_config.take().unwrap_or_else(|| serde_json::json!({}));
    s.merge_into_raw(&mut raw);
    cfg.decision_config = Some(raw);
    save_global_config(config_path, &cfg)
}

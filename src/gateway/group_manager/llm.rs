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

/// Curated notes shown to the model each turn. Default ON.
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

/// Which searchable stores a turn may read. `Default` (both off) is what a
/// caller with no settings file gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryReadSwitches {
    /// Curated notes (`memoryRecall`, default on).
    pub notes: bool,
    /// The knowledge graph (`preCognitive`, default off).
    pub graph: bool,
}

/// Both read switches from one read of the settings file. The file holds every
/// LLM config and is parsed whole, so asking for the two switches one by one
/// costs two parses on every chat turn.
pub fn get_memory_read_switches(config_path: &Path) -> MemoryReadSwitches {
    let cfg = load_global_config(config_path);
    MemoryReadSwitches {
        notes: cfg.memory_recall.unwrap_or(true),
        graph: cfg.pre_cognitive.unwrap_or(false),
    }
}

// ===== Context compression toggle (global, user-set) =====

/// Context compression of tool results. Default ON — a long JSON list or build
/// log is re-sent with every later call of the chat, so shrinking it once, where
/// it is created, is the cheapest token saving there is. The full text stays on
/// disk for the model to read back. Settings → Agent behavior turns it off.
pub fn get_context_compression_enabled(config_path: &Path) -> bool {
    load_global_config(config_path)
        .context_compression
        .unwrap_or(true)
}

pub fn save_context_compression_enabled(config_path: &Path, enabled: bool) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.context_compression = Some(enabled);
    save_global_config(config_path, &cfg)
}

/// How far a coding chat — a code session, or a chat in a git repository — runs
/// before it asks. Absent or unreadable means [`Autonomy::Auto`]: the dev loop
/// runs unattended and only what cannot be taken back asks. `ask` restores a
/// prompt for every command; `full` asks nothing.
pub fn get_code_autonomy(config_path: &Path) -> crate::zen_core::Autonomy {
    load_global_config(config_path)
        .code_autonomy
        .as_deref()
        .and_then(crate::zen_core::Autonomy::parse)
        .unwrap_or(crate::zen_core::Autonomy::Auto)
}

pub fn save_code_autonomy(config_path: &Path, level: crate::zen_core::Autonomy) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.code_autonomy = Some(level.as_str().to_string());
    save_global_config(config_path, &cfg)
}

/// What one coding request may use. Defaults: 500 tool-calling turns - the
/// backstop OpenHands uses; the stuck detector is what normally stops a loop
/// long before - and 10 million tokens, a ceiling a hosted-model bill notices
/// well before a local run does. Both can be raised, lowered or (tokens) turned
/// off with 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentLimits {
    pub code_max_turns: usize,
    pub max_run_tokens: u64,
    pub finish_judge: bool,
}

pub const DEFAULT_CODE_MAX_TURNS: usize = 500;
pub const DEFAULT_MAX_RUN_TOKENS: u64 = 10_000_000;

pub fn get_agent_limits(config_path: &Path) -> AgentLimits {
    let c = load_global_config(config_path).agent_limits.unwrap_or_default();
    AgentLimits {
        code_max_turns: c.code_max_turns.map(|n| n as usize).filter(|n| *n > 0).unwrap_or(DEFAULT_CODE_MAX_TURNS),
        max_run_tokens: c.max_run_tokens.unwrap_or(DEFAULT_MAX_RUN_TOKENS),
        finish_judge: c.finish_judge.unwrap_or(false),
    }
}

/// Change some of the limits; `None` leaves a field as it is.
pub fn save_agent_limits(
    config_path: &Path,
    code_max_turns: Option<u64>,
    max_run_tokens: Option<u64>,
    finish_judge: Option<bool>,
) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    let mut limits = cfg.agent_limits.take().unwrap_or_default();
    if code_max_turns.is_some() {
        limits.code_max_turns = code_max_turns;
    }
    if max_run_tokens.is_some() {
        limits.max_run_tokens = max_run_tokens;
    }
    if finish_judge.is_some() {
        limits.finish_judge = finish_judge;
    }
    cfg.agent_limits = Some(limits);
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

#[cfg(test)]
mod context_compression_switch_tests {
    use super::*;

    #[test]
    fn it_is_on_until_someone_turns_it_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert!(get_context_compression_enabled(&path), "no file: on");
        std::fs::write(&path, "{}").unwrap();
        assert!(get_context_compression_enabled(&path), "no key: on");
        save_context_compression_enabled(&path, false).unwrap();
        assert!(!get_context_compression_enabled(&path));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"contextCompression\": false"), "{raw}");
        save_context_compression_enabled(&path, true).unwrap();
        assert!(get_context_compression_enabled(&path));
    }

    #[test]
    fn saving_it_leaves_the_other_switches_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        save_memory_recall_enabled(&path, false).unwrap();
        save_context_compression_enabled(&path, false).unwrap();
        assert!(!get_memory_recall_enabled(&path));
        assert!(!get_context_compression_enabled(&path));
    }
}

#[cfg(test)]
mod memory_switch_tests {
    use super::*;

    #[test]
    fn with_no_settings_file_notes_are_on_and_the_graph_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // The documented defaults, read from an empty file …
        assert!(get_memory_recall_enabled(&path));
        assert!(!get_pre_cognitive_enabled(&path));
        // … and the batch read agrees with the single reads.
        assert_eq!(
            get_memory_read_switches(&path),
            MemoryReadSwitches { notes: true, graph: false }
        );
    }

    #[test]
    fn the_two_switches_are_independent_and_read_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        save_memory_recall_enabled(&path, false).unwrap();
        save_pre_cognitive_enabled(&path, true).unwrap();
        assert_eq!(
            get_memory_read_switches(&path),
            MemoryReadSwitches { notes: false, graph: true }
        );
        save_memory_recall_enabled(&path, true).unwrap();
        assert_eq!(
            get_memory_read_switches(&path),
            MemoryReadSwitches { notes: true, graph: true },
            "saving one switch must leave the other as it was"
        );
    }

    #[test]
    fn the_caller_without_a_settings_path_reads_nothing_optional() {
        assert_eq!(
            MemoryReadSwitches::default(),
            MemoryReadSwitches { notes: false, graph: false }
        );
    }
}

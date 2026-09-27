//! `GET|PUT /api/local-models/settings` (`docs/runtime-protocol.md` §5.4).
//!
//! `engine` is a byte-for-byte passthrough of `<local_models_dir>/settings.json`
//! — the file `local-model-core` and now `sen-mlx`/llama.cpp read directly, in
//! **snake_case**. The daemon never renames its fields (a `rename_all` here
//! would parse an existing file into all-`None` silently) and never
//! interprets them — sampling/KV knobs are the runtime's business.
//! `defaultContextLength` is the one setting the daemon itself owns, kept
//! next to it in a separate file so the two never fight over the same JSON.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn engine_settings_path(local_models_dir: &Path) -> std::path::PathBuf {
    local_models_dir.join("settings.json")
}

fn daemon_settings_path(local_models_dir: &Path) -> std::path::PathBuf {
    local_models_dir.join("daemon-settings.json")
}

/// The shared engine file, read as raw JSON — its shape is owned by the
/// runtimes, not the daemon.
pub fn load_engine_settings(local_models_dir: &Path) -> serde_json::Value {
    std::fs::read_to_string(engine_settings_path(local_models_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

pub fn save_engine_settings(local_models_dir: &Path, value: &serde_json::Value) -> Result<()> {
    std::fs::create_dir_all(local_models_dir)?;
    let path = engine_settings_path(local_models_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?).context("write local-models settings.json")?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DaemonModelSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_context_length: Option<u32>,
}

pub fn load_daemon_settings(local_models_dir: &Path) -> DaemonModelSettings {
    std::fs::read_to_string(daemon_settings_path(local_models_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_daemon_settings(local_models_dir: &Path, settings: &DaemonModelSettings) -> Result<()> {
    std::fs::create_dir_all(local_models_dir)?;
    let path = daemon_settings_path(local_models_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(settings)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// The context length to launch a model with (`docs/runtime-protocol.md`
/// §5.3): the request's own `contextLength` if it gave one, else
/// `min(model maximum, defaultContextLength)` — `defaultContextLength`
/// defaults to [`DEFAULT_CONTEXT_LENGTH`], never the model's full maximum, so a
/// checkpoint advertising a 128K context does not make the runtime allocate a
/// 128K KV cache the moment it loads. Used at load (`POST
/// /api/local-models/:key/load`), the JIT model route (`runtime::proxy`), and
/// to report the `local:<key>` LLM config's own `contextLength`.
/// 32K, not the 8K a chat-only default would pick: a SenClaw *agent* turn
/// opens with the system prompt plus every tool schema, measured at ~15.7K
/// tokens on a fresh install, so an 8K window fails every agent turn on a local
/// model before the model reads a word. Still capped by the model's own maximum.
pub const DEFAULT_CONTEXT_LENGTH: u32 = 32_768;

pub fn resolve_context_length(requested: Option<u32>, model_max: Option<u32>, default_context_length: Option<u32>) -> u32 {
    if let Some(explicit) = requested {
        return explicit;
    }
    let default = default_context_length.unwrap_or(DEFAULT_CONTEXT_LENGTH);
    match model_max {
        Some(max) => default.min(max),
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_request_wins_over_everything_else() {
        assert_eq!(resolve_context_length(Some(32768), Some(4096), Some(2048)), 32768);
    }

    #[test]
    fn default_context_length_caps_a_large_model_maximum() {
        // A 128K-context checkpoint must not launch with a 128K KV cache by
        // default — this is the whole point of `defaultContextLength`.
        assert_eq!(resolve_context_length(None, Some(131_072), None), DEFAULT_CONTEXT_LENGTH);
    }

    #[test]
    fn a_model_smaller_than_the_default_wins_since_it_cannot_exceed_its_own_maximum() {
        assert_eq!(resolve_context_length(None, Some(2048), None), 2048);
    }

    #[test]
    fn a_configured_default_is_still_capped_by_the_model_maximum() {
        assert_eq!(resolve_context_length(None, Some(4096), Some(16384)), 4096);
        assert_eq!(resolve_context_length(None, Some(16384), Some(4096)), 4096);
    }

    #[test]
    /// An agent turn's opening prompt (system prompt + tool schemas) was
    /// measured at ~15.7K tokens; the default window must hold it.
    #[test]
    fn the_default_window_holds_an_agent_turns_opening_prompt() {
        assert!(resolve_context_length(None, Some(131_072), None) >= 16_384);
    }

    #[test]
    fn no_known_model_maximum_falls_back_to_the_default_alone() {
        assert_eq!(resolve_context_length(None, None, Some(2048)), 2048);
        assert_eq!(resolve_context_length(None, None, None), DEFAULT_CONTEXT_LENGTH);
    }

    #[test]
    fn engine_settings_round_trip_keeps_snake_case_keys_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let value = serde_json::json!({"top_p": 0.9, "max_kv_size": 4096});
        save_engine_settings(tmp.path(), &value).unwrap();
        let text = std::fs::read_to_string(engine_settings_path(tmp.path())).unwrap();
        assert!(text.contains("top_p"), "must not rename to topP: {text}");
        assert_eq!(load_engine_settings(tmp.path()), value);
    }

    #[test]
    fn missing_engine_settings_file_defaults_to_an_empty_object() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load_engine_settings(tmp.path()), serde_json::json!({}));
    }

    #[test]
    fn daemon_settings_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = DaemonModelSettings { default_context_length: Some(8192) };
        save_daemon_settings(tmp.path(), &settings).unwrap();
        assert_eq!(load_daemon_settings(tmp.path()), settings);
    }
}

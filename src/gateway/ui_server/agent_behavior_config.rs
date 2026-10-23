//! `/api/agent-behavior` — global, user-set toggles for the process stages.
//!
//! **Pre-process stages** (run before the main agent turn):
//! - **preTriggerSkill** — deterministically match the incoming message to a
//!   skill (by triggers / when-to-use) and force-load it before the main turn.
//! - **preCognitive** — retrieve relevant cognitive-graph memory for the
//!   message and inject it into the prompt before the main turn.
//!
//! - **memoryRecall** — curated-memory stage: inject relevant curated
//!   memories (hybrid FTS5/vector search over `memory/*.md`) into the prompt
//!   before the main turn, and consolidate compaction-dropped history into
//!   curated memory files (Claude-Code-style auto-memory). Default **ON**.
//!
//! **After-process stages** (run after the main agent turn):
//! - **afterProcess** — update context by synthesising the conversation so far
//!   (Claude-Code style) so the agent retains an optimised, compact
//!   understanding of the whole dialogue for future turns. Default OFF.
//!
//! - **contextCompression** — shrink a long tool result (a JSON list, a build
//!   log) once, where it is created, instead of re-sending all of it with every
//!   later call of the chat; the full text stays on disk (`crate::context_compress`).
//!   Default **ON**.
//!
//! All five are persisted in the global config (`~/.senclaw/config.json`) and
//! read per-turn by `AgentPool`.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::Json};
use serde::Deserialize;

use crate::gateway::group_manager::{
    get_after_process_enabled, get_context_compression_enabled, get_memory_recall_enabled,
    get_pre_cognitive_enabled, get_pre_trigger_skill_enabled, save_after_process_enabled,
    save_context_compression_enabled, save_memory_recall_enabled, save_pre_cognitive_enabled,
    save_pre_trigger_skill_enabled,
};

use super::core::{AppError, UiState};

fn current(s: &UiState) -> serde_json::Value {
    let path = &s.config.paths.global_config_path;
    serde_json::json!({
        "preTriggerSkill": get_pre_trigger_skill_enabled(path),
        "preCognitive": get_pre_cognitive_enabled(path),
        "afterProcess": get_after_process_enabled(path),
        "memoryRecall": get_memory_recall_enabled(path),
        "contextCompression": get_context_compression_enabled(path),
    })
}

pub(crate) async fn agent_behavior_get(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    Json(current(&s))
}

/// Partial update — only the fields present in the body are changed, so the UI
/// can toggle any switch independently.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct AgentBehaviorBody {
    #[serde(rename = "preTriggerSkill")]
    pre_trigger_skill: Option<bool>,
    #[serde(rename = "preCognitive")]
    pre_cognitive: Option<bool>,
    #[serde(rename = "afterProcess")]
    after_process: Option<bool>,
    #[serde(rename = "memoryRecall")]
    memory_recall: Option<bool>,
    #[serde(rename = "contextCompression")]
    context_compression: Option<bool>,
}

pub(crate) async fn agent_behavior_set(
    State(s): State<Arc<UiState>>,
    Json(body): Json<AgentBehaviorBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let path = &s.config.paths.global_config_path;
    if let Some(v) = body.pre_trigger_skill {
        save_pre_trigger_skill_enabled(path, v)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    if let Some(v) = body.pre_cognitive {
        save_pre_cognitive_enabled(path, v)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    if let Some(v) = body.after_process {
        save_after_process_enabled(path, v)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    if let Some(v) = body.memory_recall {
        save_memory_recall_enabled(path, v)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    if let Some(v) = body.context_compression {
        save_context_compression_enabled(path, v)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(current(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;

    fn state(config_path: &std::path::Path) -> Arc<UiState> {
        let mut cfg = Config::from_env();
        cfg.paths.global_config_path = config_path.to_path_buf();
        Arc::new(UiState {
            config: Arc::new(cfg),
            db: None,
            group_manager: None,
            wiki_manager: None,
            persona_registry: None,
            agent_api: None,
            mcp_manager: None,
            marketplace_manager: None,
            workbench_bridge: None,
            dispatch_bridge: None,
            space_mcp_launcher: None,
            workflow_service: None,
            virtual_worker_pool: None,
            background_scheduler: None,
            agent_states: None,
            usage_recorder: None,
            checkpoints: None,
            runtime_manager: None,
            ws_port: 0,
            ws_token: String::new(),
            api_auth: Arc::new(super::super::auth::ApiAuth::disabled()),
        })
    }

    /// A client's JSON, through the handler's own body type — the wire names
    /// are the part a rename typo breaks without failing to compile.
    async fn post(s: &Arc<UiState>, body: serde_json::Value) -> serde_json::Value {
        let body: AgentBehaviorBody = serde_json::from_value(body).unwrap();
        match agent_behavior_set(State(s.clone()), Json(body)).await {
            Ok(Json(v)) => v,
            Err(_) => panic!("agent_behavior_set failed"),
        }
    }

    #[tokio::test]
    async fn context_compression_is_on_until_switched_off_and_the_switch_sticks() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let s = state(&config_path);

        // No config file: on by default, the other switches at theirs.
        let Json(v) = agent_behavior_get(State(s.clone())).await;
        assert_eq!(v["contextCompression"], true, "{v}");
        assert_eq!((v["memoryRecall"].as_bool(), v["afterProcess"].as_bool()), (Some(true), Some(false)));

        // Off: answered, persisted, and what the next tool result will read.
        let v = post(&s, json!({"contextCompression": false})).await;
        assert_eq!(v["contextCompression"], false);
        assert!(!crate::gateway::group_manager::get_context_compression_enabled(&config_path));
        assert!(std::fs::read_to_string(&config_path).unwrap().contains("\"contextCompression\": false"));

        // The desktop sends the whole object back: nothing else changes.
        let v = post(
            &s,
            json!({"afterProcess": false, "memoryRecall": true, "contextCompression": false, "preCognitive": false, "preTriggerSkill": false}),
        )
        .await;
        assert_eq!((v["memoryRecall"].as_bool(), v["contextCompression"].as_bool()), (Some(true), Some(false)));

        // Another switch alone leaves it as it was; back on works.
        let v = post(&s, json!({"memoryRecall": false})).await;
        assert_eq!((v["memoryRecall"].as_bool(), v["contextCompression"].as_bool()), (Some(false), Some(false)));
        let v = post(&s, json!({"contextCompression": true})).await;
        assert_eq!(v["contextCompression"], true);
        assert!(crate::gateway::group_manager::get_context_compression_enabled(&config_path));
    }
}

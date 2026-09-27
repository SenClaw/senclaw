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
//!   curated memory files (Claude-Code-style auto-memory).
//!
//! **After-process stages** (run after the main agent turn):
//! - **afterProcess** — update context by synthesising the conversation so far
//!   (Claude-Code style) so the agent retains an optimised, compact
//!   understanding of the whole dialogue for future turns.
//!
//! All four are persisted in the global config (`~/.senclaw/config.json`) and
//! read per-turn by `AgentPool`.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::Json};
use serde::Deserialize;

use crate::gateway::group_manager::{
    get_after_process_enabled, get_memory_recall_enabled, get_pre_cognitive_enabled,
    get_pre_trigger_skill_enabled, save_after_process_enabled, save_memory_recall_enabled,
    save_pre_cognitive_enabled, save_pre_trigger_skill_enabled,
};

use super::core::{AppError, UiState};

fn current(s: &UiState) -> serde_json::Value {
    let path = &s.config.paths.global_config_path;
    serde_json::json!({
        "preTriggerSkill": get_pre_trigger_skill_enabled(path),
        "preCognitive": get_pre_cognitive_enabled(path),
        "afterProcess": get_after_process_enabled(path),
        "memoryRecall": get_memory_recall_enabled(path),
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
    Ok(Json(current(&s)))
}

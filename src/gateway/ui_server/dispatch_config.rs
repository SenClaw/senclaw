//! `/api/dispatch-config` — the autonomous MCP dispatcher toggle.
//!
//! When `enabled`, the `MCPDispatcher` (src/agent/mcp_dispatch) claims ready
//! tasks from dispatch sources (the Kanban board) and runs a persona worker
//! agent for each. Persisted in the global config (`~/.senclaw/config.json`) and
//! read by the dispatcher each poll tick, so the toggle takes effect live
//! (within one poll interval) without a daemon restart. Default OFF.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::Json};
use serde::Deserialize;

use crate::gateway::group_manager::{get_dispatch_enabled, save_dispatch_enabled};

use super::core::{AppError, UiState};

pub(crate) async fn dispatch_config_get(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let path = &s.config.paths.global_config_path;
    Json(serde_json::json!({ "enabled": get_dispatch_enabled(path) }))
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DispatchConfigBody {
    enabled: Option<bool>,
}

pub(crate) async fn dispatch_config_set(
    State(s): State<Arc<UiState>>,
    Json(body): Json<DispatchConfigBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let path = &s.config.paths.global_config_path;
    if let Some(v) = body.enabled {
        save_dispatch_enabled(path, v)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(
        serde_json::json!({ "enabled": get_dispatch_enabled(path) }),
    ))
}

//! `/api/chats/:jid/trajectory*` — the recorded turns of a chat, for the
//! replay panel and the evals runner.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use super::core::{AppError, UiState};
use crate::trajectory;

/// GET /api/chats/:jid/trajectory
pub(crate) async fn trajectory_list(
    State(_s): State<Arc<UiState>>,
    Path(jid): Path<String>,
) -> Json<serde_json::Value> {
    let turns = trajectory::list_turns(&jid);
    Json(serde_json::json!({
        "enabled": trajectory::is_enabled(&jid),
        "dir": trajectory::chat_dir(&jid).to_string_lossy(),
        "turns": turns,
    }))
}

/// GET /api/chats/:jid/trajectory/:turn
pub(crate) async fn trajectory_turn(
    State(_s): State<Arc<UiState>>,
    Path((jid, turn)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let lines = trajectory::read_turn(&jid, &turn)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "turn not found".into()))?;
    Ok(Json(serde_json::json!({ "turnId": turn, "messages": lines })))
}

#[derive(Deserialize)]
pub(crate) struct SettingsBody {
    enabled: bool,
}

/// PUT /api/chats/:jid/trajectory/settings
pub(crate) async fn trajectory_settings(
    State(_s): State<Arc<UiState>>,
    Path(jid): Path<String>,
    Json(body): Json<SettingsBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    trajectory::set_enabled(&jid, body.enabled)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true, "enabled": body.enabled })))
}

/// DELETE /api/chats/:jid/trajectory — remove every recorded turn.
pub(crate) async fn trajectory_delete(
    State(_s): State<Arc<UiState>>,
    Path(jid): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let n = trajectory::delete_all(&jid)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true, "removed": n })))
}

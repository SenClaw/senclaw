//! Workbench HTTP handlers — reverse operations from the WebUI to the
//! per-group engine's `WorkbenchService` via [`WorkbenchBridge`].
//!
//! Endpoints:
//!   - POST  /api/workbench/:jid/:id/mark-viewed
//!   - POST  /api/workbench/:jid/:id/close
//!   - GET   /api/workbench/:jid/:id/read-file?path=<rel>
//!   - GET   /api/workbench/:jid/:id/logs?tail=<n>

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use super::core::{AppError, UiState};

fn bridge(
    state: &Arc<UiState>,
) -> Result<Arc<crate::agent::workbench_bridge::WorkbenchBridge>, AppError> {
    state.workbench_bridge.clone().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "workbench_bridge_unset".into(),
        )
    })
}

pub(crate) async fn workbench_mark_viewed(
    State(s): State<Arc<UiState>>,
    Path((chat_jid, artifact_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let bridge = bridge(&s)?;
    let ok = bridge.mark_viewed(&chat_jid, &artifact_id);
    Ok(Json(serde_json::json!({ "ok": ok })))
}

pub(crate) async fn workbench_close(
    State(s): State<Arc<UiState>>,
    Path((chat_jid, artifact_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let bridge = bridge(&s)?;
    let ok = bridge.close(&chat_jid, &artifact_id);
    Ok(Json(serde_json::json!({ "ok": ok })))
}

#[derive(Deserialize)]
pub(crate) struct ReadFileQuery {
    pub path: String,
}

pub(crate) async fn workbench_read_file(
    State(s): State<Arc<UiState>>,
    Path((chat_jid, artifact_id)): Path<(String, String)>,
    Query(q): Query<ReadFileQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let bridge = bridge(&s)?;
    match bridge.read_file(&chat_jid, &artifact_id, &q.path) {
        Ok(content) => Ok(Json(serde_json::json!({ "content": content }))),
        Err(err) => Ok(Json(serde_json::json!({ "error": err }))),
    }
}

#[derive(Deserialize, Default)]
pub(crate) struct LogsQuery {
    pub tail: Option<usize>,
}

pub(crate) async fn workbench_fetch_logs(
    State(s): State<Arc<UiState>>,
    Path((chat_jid, artifact_id)): Path<(String, String)>,
    Query(q): Query<LogsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let bridge = bridge(&s)?;
    let tail = q.tail.unwrap_or(200);
    let logs = bridge.fetch_logs(&chat_jid, &artifact_id, tail);
    Ok(Json(serde_json::json!({ "logs": logs })))
}

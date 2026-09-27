//! Read-only dispatch snapshot.
//!
//! The DAG dispatch tree normally reaches clients as the `dispatch:update`
//! WebSocket event, which `WsGateway::notify_dispatch_update` sends with
//! `broadcast_to_admins` — WebSocket admin clients and nobody else. A mobile
//! client reaches the daemon only over the relay's `/api/*` tunnel, so it can
//! never receive that event.
//!
//! Forwarding the event to app channels was the other option and was not
//! taken: it would push the whole parents tree to every paired device on every
//! state mutation whether or not anything is looking at it, and it would still
//! be lossy — the relay's reconnect cycle drops events, so a client would show
//! a half-built tree after any drop and have no way to reconcile. A snapshot
//! the caller polls is both cheaper and correct, and it is the pattern the
//! mobile app already uses for the other admin-only event family (`bg:*`).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::core::{AppError, UiState};

fn bridge(
    s: &Arc<UiState>,
) -> Result<Arc<crate::agent::dispatch_bridge::DispatchBridge>, AppError> {
    s.dispatch_bridge.clone().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "dispatch_bridge_unset".into(),
        )
    })
}

/// GET /api/dispatch — the parents array, identical in shape to
/// `dispatch:update.parents`.
pub(crate) async fn dispatch_snapshot(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let parents = bridge(&s)?.parents_snapshot();
    Ok(Json(serde_json::json!({ "parents": parents })))
}

/// POST /api/dispatch/tasks/:task_id/retry — re-run one failed subtask.
///
/// A refusal (already done, still running, unknown id) is a `400` carrying the
/// reason verbatim: the UI shows it to the person who clicked, and a generic
/// "retry failed" would leave them clicking again.
pub(crate) async fn dispatch_retry_task(
    State(s): State<Arc<UiState>>,
    Path(task_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    match bridge(&s)?.retry_task(&task_id) {
        Ok(label) => Ok(Json(serde_json::json!({
            "success": true,
            "taskId": task_id,
            "label": label,
        }))),
        Err(reason) => Err(AppError(StatusCode::BAD_REQUEST, reason)),
    }
}

/// POST /api/dispatch/parents/:parent_id/retry — re-run every failed subtask.
pub(crate) async fn dispatch_retry_parent(
    State(s): State<Arc<UiState>>,
    Path(parent_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    match bridge(&s)?.retry_parent_failed(&parent_id) {
        Ok(labels) => Ok(Json(serde_json::json!({
            "success": true,
            "parentId": parent_id,
            "retried": labels.len(),
            "labels": labels,
        }))),
        Err(reason) => Err(AppError(StatusCode::BAD_REQUEST, reason)),
    }
}

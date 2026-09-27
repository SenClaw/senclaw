//! `/api/failures` — the failure ledger: what tools failed, how often the same
//! failure came back, and whether it was ever fixed.
//!
//! Read-only, and deliberately so. The ledger is written by
//! [`crate::failures`] off the engine event bus and nothing feeds it back into
//! a prompt; these two endpoints exist to answer whether it is worth building
//! anything that does. See [docs/failure-ledger.md].

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::core::{AppError, UiState};

fn db(s: &Arc<UiState>) -> Result<Arc<crate::db::Db>, AppError> {
    s.db.clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "db_unset".into()))
}

/// A `Query` field's name is its wire name — camelCase with a snake_case alias,
/// the same shape `/api/watches` needs.
#[derive(serde::Deserialize)]
pub(crate) struct ListQuery {
    #[serde(default, rename = "chatJid", alias = "chat_jid")]
    chat_jid: Option<String>,
    /// `open` | `resolved` | `gave_up`.
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

/// GET /api/failures?chatJid=&status=&limit= — recent episodes, newest first.
pub(crate) async fn failures_list(
    State(s): State<Arc<UiState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let rows = db
        .list_failure_episodes(q.chat_jid.as_deref(), q.status.as_deref(), limit)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "count": rows.len(),
        "episodes": rows,
    })))
}

#[derive(serde::Deserialize)]
pub(crate) struct SummaryQuery {
    #[serde(default)]
    days: Option<i64>,
}

/// GET /api/failures/summary?days=30 — the three numbers that decide whether a
/// lesson store is worth building: how often a failure repeats, how often it
/// gets fixed, and who fixed it.
pub(crate) async fn failures_summary(
    State(s): State<Arc<UiState>>,
    Query(q): Query<SummaryQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let days = q.days.unwrap_or(30).clamp(1, 365);
    let since = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    let out = db
        .failure_summary(&since)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(out))
}

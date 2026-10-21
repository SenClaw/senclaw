//! `/api/skill-usage` — the skill usage ledger: which skills get loaded, how
//! the turn ended, and how often tools failed meanwhile.
//!
//! Read-only. The ledger is written by [`crate::skills::usage`] off the engine
//! event bus and nothing feeds it back into a prompt; these endpoints exist to
//! answer whether any skill misbehaves often enough to be worth changing. See
//! [docs/skill-history.md].

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::core::{AppError, UiState};

fn db(s: &Arc<UiState>) -> Result<Arc<crate::db::Db>, AppError> {
    s.db.clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "db_unset".into()))
}

#[derive(serde::Deserialize)]
pub(crate) struct ListQuery {
    /// A skill's directory key or name.
    #[serde(default)]
    skill: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// GET /api/skill-usage?skill=&limit= — recent loads, newest first.
pub(crate) async fn skill_usage_list(
    State(s): State<Arc<UiState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let rows = db
        .list_skill_usage(q.skill.as_deref(), q.limit.unwrap_or(100).clamp(1, 1000))
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(
        serde_json::json!({ "count": rows.len(), "loads": rows }),
    ))
}

#[derive(serde::Deserialize)]
pub(crate) struct SummaryQuery {
    #[serde(default)]
    days: Option<i64>,
}

/// GET /api/skill-usage/summary?days=30 — per skill and per content version:
/// loads, how the turns ended, `fallbackRate` and `toolFailureRate`.
pub(crate) async fn skill_usage_summary(
    State(s): State<Arc<UiState>>,
    Query(q): Query<SummaryQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let days = q.days.unwrap_or(30).clamp(1, 365);
    let since = (chrono::Utc::now() - chrono::Duration::days(days)).timestamp_millis();
    let mut out = crate::skills::usage::summary(&db, since)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    out["days"] = serde_json::json!(days);
    Ok(Json(out))
}

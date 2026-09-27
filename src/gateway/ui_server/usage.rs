//! REST surface for token accounting: `/api/usage/*`.
//!
//! Overview/breakdown read the raw `llm_usage_log` (live, bounded by the
//! 90-day retention); the daily chart reads the `llm_usage_daily` rollup.
//! Costs come from `model_pricing` with prefix matching; unpriced volume is
//! reported as `unpricedTokens`, never silently costed at $0.

use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::Json,
};
use serde::Deserialize;

use super::core::{AppError, UiState};
use crate::db::usage::{ModelPricing, BREAKDOWN_KEYS};
use crate::db::Db;

fn need_db(s: &Arc<UiState>) -> Result<Arc<Db>, AppError> {
    s.db.clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))
}

fn internal(e: anyhow::Error) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// GET /api/usage/overview — totals for today (UTC), last 7 days, last 30 days.
pub(crate) async fn usage_overview(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = need_db(&s)?;
    let now = chrono::Utc::now();
    let until = now.timestamp_millis() + 60_000;
    let today_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc()
        .timestamp_millis();
    let week_start = (now - chrono::Duration::days(7)).timestamp_millis();
    let month_start = (now - chrono::Duration::days(30)).timestamp_millis();

    let today = db.usage_totals(today_start, until).map_err(internal)?;
    let week = db.usage_totals(week_start, until).map_err(internal)?;
    let month = db.usage_totals(month_start, until).map_err(internal)?;
    Ok(Json(serde_json::json!({
        "today": today, "week": week, "month": month,
    })))
}

#[derive(Deserialize)]
pub(crate) struct DailyQuery {
    #[serde(default = "default_days")]
    days: u32,
}

fn default_days() -> u32 {
    30
}

/// GET /api/usage/daily?days=30 — per-day rollup rows, oldest first.
pub(crate) async fn usage_daily(
    State(s): State<Arc<UiState>>,
    Query(q): Query<DailyQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = need_db(&s)?;
    let days = q.days.clamp(1, 365);
    let rows = db.usage_daily(days).map_err(internal)?;
    Ok(Json(serde_json::json!({ "days": days, "rows": rows })))
}

#[derive(Deserialize)]
pub(crate) struct BreakdownQuery {
    #[serde(default = "default_by")]
    by: String,
    #[serde(default = "default_breakdown_days")]
    days: u32,
}

fn default_by() -> String {
    "model".into()
}

fn default_breakdown_days() -> u32 {
    7
}

/// GET /api/usage/breakdown?by=model|source|jid|app&days=7
pub(crate) async fn usage_breakdown(
    State(s): State<Arc<UiState>>,
    Query(q): Query<BreakdownQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = need_db(&s)?;
    // "app" is the friendly alias for the app_id column.
    let by = if q.by == "app" { "app_id" } else { q.by.as_str() };
    if !BREAKDOWN_KEYS.contains(&by) {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("invalid 'by' (expected one of model, source, jid, app): {}", q.by),
        ));
    }
    let days = q.days.clamp(1, 90);
    let since = now_ms() - (days as i64) * 86_400_000;
    let rows = db
        .usage_breakdown(by, since, now_ms() + 60_000)
        .map_err(internal)?;
    Ok(Json(serde_json::json!({ "by": q.by, "days": days, "rows": rows })))
}

#[derive(Deserialize)]
pub(crate) struct LogQuery {
    #[serde(default = "default_limit")]
    limit: u32,
    before: Option<i64>,
}

fn default_limit() -> u32 {
    100
}

/// GET /api/usage/log?limit=100&before=<id> — raw rows, newest first.
pub(crate) async fn usage_log(
    State(s): State<Arc<UiState>>,
    Query(q): Query<LogQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = need_db(&s)?;
    let rows = db
        .usage_log_recent(q.limit.clamp(1, 500), q.before)
        .map_err(internal)?;
    let next_before = rows.last().map(|r| r.id);
    Ok(Json(serde_json::json!({ "rows": rows, "nextBefore": next_before })))
}

/// GET /api/usage/pricing — all pricing rows.
pub(crate) async fn pricing_list(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = need_db(&s)?;
    let rows = db.usage_pricing_all().map_err(internal)?;
    Ok(Json(serde_json::json!({ "rows": rows })))
}

/// PUT /api/usage/pricing — upsert one row (body = ModelPricing JSON).
pub(crate) async fn pricing_upsert(
    State(s): State<Arc<UiState>>,
    Json(p): Json<ModelPricing>,
) -> Result<Json<serde_json::Value>, AppError> {
    if p.model.trim().is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "model is required".into()));
    }
    let db = need_db(&s)?;
    db.usage_pricing_upsert(&p).map_err(internal)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/usage/pricing/:model
pub(crate) async fn pricing_delete(
    State(s): State<Arc<UiState>>,
    AxumPath(model): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = need_db(&s)?;
    let removed = db.usage_pricing_delete(&model).map_err(internal)?;
    Ok(Json(serde_json::json!({ "ok": true, "removed": removed })))
}

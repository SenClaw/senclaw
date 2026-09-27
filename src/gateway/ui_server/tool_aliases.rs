//! MCP tool alias REST API (Plugins → Alias).
//!
//! Endpoints:
//!   GET    /api/tool-aliases                 — list every alias (all sources)
//!   POST   /api/tool-aliases                 — create a user alias { alias, target, description? }
//!   PUT    /api/tool-aliases/:alias          — update target/description (user aliases only)
//!   POST   /api/tool-aliases/:alias/enabled  — toggle { enabled } — the approval gate for app aliases
//!   DELETE /api/tool-aliases/:alias          — delete (an app alias re-imports disabled on next app start)
//!
//! Every mutation reloads the process-wide registry consumed by
//! `resolve_tool_by_name`, so changes apply from the next agent turn — no
//! daemon restart needed.

use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::Deserialize;
use serde_json::json;

use crate::db::tool_aliases::SOURCE_USER;
use crate::db::Db;

use super::core::{AppError, UiState};

fn db(s: &UiState) -> Result<Arc<Db>, AppError> {
    s.db.clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))
}

fn internal(e: anyhow::Error) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// Validate one side of a mapping: non-empty, no whitespace.
fn check_name(label: &str, value: &str) -> Result<String, AppError> {
    let v = value.trim();
    if v.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("{label} is required"),
        ));
    }
    if v.contains(char::is_whitespace) {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("{label} must not contain whitespace"),
        ));
    }
    Ok(v.to_string())
}

pub(crate) async fn aliases_list(
    State(s): State<Arc<UiState>>,
) -> Result<impl IntoResponse, AppError> {
    let db = db(&s)?;
    let aliases = db.list_tool_aliases().map_err(internal)?;
    Ok(Json(json!({ "aliases": aliases })))
}

#[derive(Deserialize)]
pub(crate) struct AliasCreateBody {
    alias: String,
    target: String,
    #[serde(default)]
    description: Option<String>,
    /// User aliases default to enabled; pass `false` to create disabled.
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

pub(crate) async fn aliases_create(
    State(s): State<Arc<UiState>>,
    Json(body): Json<AliasCreateBody>,
) -> Result<impl IntoResponse, AppError> {
    let alias = check_name("alias", &body.alias)?;
    let target = check_name("target", &body.target)?;
    if alias == target {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "alias and target must differ".into(),
        ));
    }
    let db = db(&s)?;
    let created = db
        .create_tool_alias(
            &alias,
            &target,
            body.description.as_deref().map(str::trim).filter(|d| !d.is_empty()),
            body.enabled,
            SOURCE_USER,
        )
        .map_err(internal)?;
    if !created {
        return Err(AppError(
            StatusCode::CONFLICT,
            format!("alias '{alias}' already exists"),
        ));
    }
    crate::tools::tool_alias::reload_from_db(&db);
    let row = db.get_tool_alias(&alias).map_err(internal)?;
    Ok(Json(json!({ "ok": true, "alias": row })))
}

#[derive(Deserialize)]
pub(crate) struct AliasUpdateBody {
    target: String,
    #[serde(default)]
    description: Option<String>,
}

pub(crate) async fn aliases_update(
    State(s): State<Arc<UiState>>,
    AxumPath(alias): AxumPath<String>,
    Json(body): Json<AliasUpdateBody>,
) -> Result<impl IntoResponse, AppError> {
    let target = check_name("target", &body.target)?;
    if alias == target {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "alias and target must differ".into(),
        ));
    }
    let db = db(&s)?;
    let existing = db
        .get_tool_alias(&alias)
        .map_err(internal)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "alias not found".into()))?;
    if existing.source != SOURCE_USER {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!(
                "alias is managed by '{}' — its target comes from the app manifest; you can only enable/disable or delete it",
                existing.source
            ),
        ));
    }
    db.update_tool_alias(
        &alias,
        &target,
        body.description.as_deref().map(str::trim).filter(|d| !d.is_empty()),
    )
    .map_err(internal)?;
    crate::tools::tool_alias::reload_from_db(&db);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct AliasEnabledBody {
    enabled: bool,
}

pub(crate) async fn aliases_set_enabled(
    State(s): State<Arc<UiState>>,
    AxumPath(alias): AxumPath<String>,
    Json(body): Json<AliasEnabledBody>,
) -> Result<impl IntoResponse, AppError> {
    let db = db(&s)?;
    let updated = db
        .set_tool_alias_enabled(&alias, body.enabled)
        .map_err(internal)?;
    if !updated {
        return Err(AppError(StatusCode::NOT_FOUND, "alias not found".into()));
    }
    crate::tools::tool_alias::reload_from_db(&db);
    Ok(Json(json!({ "ok": true, "alias": alias, "enabled": body.enabled })))
}

pub(crate) async fn aliases_delete(
    State(s): State<Arc<UiState>>,
    AxumPath(alias): AxumPath<String>,
) -> Result<impl IntoResponse, AppError> {
    let db = db(&s)?;
    let deleted = db.delete_tool_alias(&alias).map_err(internal)?;
    if !deleted {
        return Err(AppError(StatusCode::NOT_FOUND, "alias not found".into()));
    }
    crate::tools::tool_alias::reload_from_db(&db);
    Ok(Json(json!({ "ok": true })))
}

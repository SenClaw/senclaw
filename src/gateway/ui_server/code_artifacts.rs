//! Code artifact REST API — publish & reuse code snippets from the Code REPL.
//!
//! Endpoints (under `/api/code/artifacts`):
//!   GET    /api/code/artifacts          — list (newest first)
//!   POST   /api/code/artifacts          — create { name, language, code, description?, tags? }
//!   GET    /api/code/artifacts/:id       — fetch one
//!   PUT    /api/code/artifacts/:id       — update
//!   DELETE /api/code/artifacts/:id       — delete
//!   POST   /api/code/artifacts/:id/run   — run the saved snippet, return the outcome

use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::db::code_artifacts::CodeArtifact;
use crate::db::Db;
use crate::util::local_time::local_iso_string_now;

use super::core::{AppError, UiState};

const RUN_TIMEOUT_MS: u64 = 5_000;
/// Ceiling for a caller-supplied `timeout_ms` — generous enough for slow
/// scripts (or a debug-build sandbox child on a loaded machine), bounded so a
/// stuck run cannot pin the executor indefinitely.
const RUN_TIMEOUT_MAX_MS: u64 = 120_000;
const RUN_MEMORY_MB: u64 = 128;

fn db(s: &UiState) -> Result<Arc<Db>, AppError> {
    s.db.clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))
}

/// Normalize a language string to one of the supported canonical values.
fn normalize_language(lang: &str) -> Option<&'static str> {
    match lang.trim().to_lowercase().as_str() {
        "javascript" | "js" => Some("javascript"),
        "typescript" | "ts" => Some("typescript"),
        "bash" | "sh" | "shell" => Some("bash"),
        _ => None,
    }
}

pub(crate) async fn list_artifacts(
    State(s): State<Arc<UiState>>,
) -> Result<impl IntoResponse, AppError> {
    let db = db(&s)?;
    let artifacts = db
        .list_code_artifacts()
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(json!({ "artifacts": artifacts })))
}

#[derive(Deserialize)]
pub(crate) struct ArtifactBody {
    name: String,
    language: String,
    code: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tags: Vec<String>,
}

pub(crate) async fn create_artifact(
    State(s): State<Arc<UiState>>,
    Json(body): Json<ArtifactBody>,
) -> Result<impl IntoResponse, AppError> {
    if body.name.trim().is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "name is required".into()));
    }
    if body.code.trim().is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "code is required".into()));
    }
    let language = normalize_language(&body.language).ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            format!("unsupported language `{}`", body.language),
        )
    })?;

    let db = db(&s)?;
    let now = local_iso_string_now();
    let artifact = CodeArtifact {
        id: Uuid::new_v4().to_string(),
        name: body.name.trim().to_string(),
        language: language.to_string(),
        code: body.code,
        description: body.description.trim().to_string(),
        tags: body.tags,
        created_at: now.clone(),
        updated_at: now,
    };
    db.insert_code_artifact(&artifact)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(Json(artifact))
}

pub(crate) async fn get_artifact(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<impl IntoResponse, AppError> {
    let db = db(&s)?;
    let artifact = db
        .get_code_artifact(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "artifact not found".into()))?;
    Ok(Json(artifact))
}

pub(crate) async fn update_artifact(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<ArtifactBody>,
) -> Result<impl IntoResponse, AppError> {
    if body.name.trim().is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "name is required".into()));
    }
    let language = normalize_language(&body.language).ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            format!("unsupported language `{}`", body.language),
        )
    })?;
    let db = db(&s)?;
    let updated = db
        .update_code_artifact(
            &id,
            body.name.trim(),
            language,
            &body.code,
            body.description.trim(),
            &body.tags,
            &local_iso_string_now(),
        )
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    if !updated {
        return Err(AppError(StatusCode::NOT_FOUND, "artifact not found".into()));
    }
    Ok(Json(json!({ "ok": true })))
}

pub(crate) async fn delete_artifact(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<impl IntoResponse, AppError> {
    let db = db(&s)?;
    let deleted = db
        .delete_code_artifact(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    if !deleted {
        return Err(AppError(StatusCode::NOT_FOUND, "artifact not found".into()));
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize, Default)]
pub(crate) struct RunArtifactReq {
    /// Wall-clock budget for this run. Defaults to [`RUN_TIMEOUT_MS`], capped
    /// at [`RUN_TIMEOUT_MAX_MS`]. The body itself is optional — a bare POST
    /// keeps the old behavior.
    timeout_ms: Option<u64>,
}

pub(crate) async fn run_artifact(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<RunArtifactReq>>,
) -> Result<impl IntoResponse, AppError> {
    let db = db(&s)?;
    let artifact = db
        .get_code_artifact(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "artifact not found".into()))?;

    let timeout_ms = body
        .and_then(|Json(r)| r.timeout_ms)
        .unwrap_or(RUN_TIMEOUT_MS)
        .clamp(1_000, RUN_TIMEOUT_MAX_MS);
    let value = super::code::run_code(
        Some(&artifact.language),
        artifact.code,
        timeout_ms,
        RUN_MEMORY_MB,
    )
    .await
    .map_err(|e| AppError(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(value))
}

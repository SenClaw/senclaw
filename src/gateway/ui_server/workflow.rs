//! Workflow REST API — list definitions, run history, trigger + cancel runs.
//!
//! Thin HTTP layer over `crate::workflow::WorkflowService` (registry + run
//! store + DAG executor). State pushes also flow over WS as
//! `workflow:update`; these endpoints serve initial fetch and CLI-less
//! clients.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use super::core::UiState;

fn service(
    state: &UiState,
) -> Result<Arc<crate::workflow::WorkflowService>, (StatusCode, Json<serde_json::Value>)> {
    state.workflow_service.clone().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workflow service not available"})),
    ))
}

/// GET /api/workflows — definition summaries (re-scans the workflows dir).
pub(crate) async fn workflows_list(
    State(state): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    let defs = tokio::task::spawn_blocking(move || svc.list_defs())
        .await
        .map_err(internal)?;
    Ok(Json(serde_json::json!({ "workflows": defs })))
}

/// GET /api/workflows/runs — run history, newest first.
pub(crate) async fn workflows_runs(
    State(state): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    Ok(Json(serde_json::json!({ "runs": svc.list_runs() })))
}

/// GET /api/workflows/runs/:id — one run record.
pub(crate) async fn workflows_run_get(
    State(state): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    match svc.get_run(&id) {
        Some(run) => Ok(Json(serde_json::json!({ "run": run }))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("run \"{id}\" not found")})),
        )),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct StartRunBody {
    #[serde(default)]
    pub inputs: HashMap<String, String>,
}

/// POST /api/workflows/:name/run — fire-and-forget trigger; returns runId.
pub(crate) async fn workflows_run_start(
    State(state): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
    body: Option<Json<StartRunBody>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    let inputs = body.map(|Json(b)| b.inputs).unwrap_or_default();
    let started =
        tokio::task::spawn_blocking(move || svc.start_run(&name, inputs, Some("ui".to_string())))
            .await
            .map_err(internal)?;
    match started {
        Ok(run_id) => Ok(Json(serde_json::json!({ "runId": run_id }))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )),
    }
}

/// GET /api/workflows/runs/:id/activity — live agent activity (think/tool…).
pub(crate) async fn workflows_run_activity(
    State(state): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    Ok(Json(
        serde_json::json!({ "entries": svc.run_activity(&id) }),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct RenameRunBody {
    #[serde(default)]
    pub label: String,
}

/// PATCH /api/workflows/runs/:id — rename (display label; empty clears).
pub(crate) async fn workflows_run_rename(
    State(state): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<RenameRunBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    match svc.rename_run(&id, &body.label) {
        Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(e) => Err((StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e})))),
    }
}

/// DELETE /api/workflows/runs/:id — drop the record (workspace kept).
pub(crate) async fn workflows_run_delete(
    State(state): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    match svc.delete_run(&id) {
        Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(e) => {
            let code = if e.contains("still running") {
                StatusCode::CONFLICT
            } else {
                StatusCode::NOT_FOUND
            };
            Err((code, Json(serde_json::json!({"error": e}))))
        }
    }
}

/// POST /api/workflows/runs/:id/cancel — stop dispatching + abort in-flight steps.
pub(crate) async fn workflows_run_cancel(
    State(state): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    let cancelled = svc.cancel(&id);
    if cancelled {
        Ok(Json(serde_json::json!({ "ok": true })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("run \"{id}\" is not active")})),
        ))
    }
}

// ===== Definition CRUD =====

/// GET /api/workflows/:name/definition — raw markdown (edit/export).
pub(crate) async fn workflows_def_get(
    State(state): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    let found = tokio::task::spawn_blocking(move || svc.get_definition(&name))
        .await
        .map_err(internal)?;
    match found {
        Some((file_name, content)) => Ok(Json(serde_json::json!({
            "fileName": file_name,
            "content": content,
        }))),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "workflow not found"})),
        )),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct DefinitionBody {
    pub content: String,
    /// Create only: replace an existing workflow with the same name (import).
    #[serde(default)]
    pub overwrite: bool,
}

/// POST /api/workflows — create a new definition (also the import path).
pub(crate) async fn workflows_def_create(
    State(state): State<Arc<UiState>>,
    Json(body): Json<DefinitionBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    let created =
        tokio::task::spawn_blocking(move || svc.create_definition(&body.content, body.overwrite))
            .await
            .map_err(internal)?;
    match created {
        Ok(name) => Ok(Json(serde_json::json!({ "name": name }))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )),
    }
}

/// PUT /api/workflows/:name/definition — overwrite an existing definition.
pub(crate) async fn workflows_def_update(
    State(state): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<DefinitionBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    let updated = tokio::task::spawn_blocking(move || svc.update_definition(&name, &body.content))
        .await
        .map_err(internal)?;
    match updated {
        Ok(new_name) => Ok(Json(serde_json::json!({ "name": new_name }))),
        Err(e) => {
            let code = if e.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            Err((code, Json(serde_json::json!({"error": e}))))
        }
    }
}

/// DELETE /api/workflows/:name — remove the definition file (runs/workspace kept).
pub(crate) async fn workflows_def_delete(
    State(state): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    let deleted = tokio::task::spawn_blocking(move || svc.delete_definition(&name))
        .await
        .map_err(internal)?;
    match deleted {
        Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(e) => Err((StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e})))),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct DraftBody {
    pub description: String,
}

/// POST /api/workflows/draft — one-shot agent authors a draft definition
/// from a natural-language description. Nothing is written to disk; the UI
/// shows the draft in the editor and the user saves via POST /api/workflows.
pub(crate) async fn workflows_draft(
    State(state): State<Arc<UiState>>,
    Json(body): Json<DraftBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    match svc.draft_definition(&body.description).await {
        Ok((content, name)) => Ok(Json(serde_json::json!({
            "name": name,
            "content": content,
        }))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )),
    }
}

/// PATCH /api/workflows/:name/definition — targeted guidance/timeout edit
/// (the tune form). Body: { guidance?, steps: [{id, guidance?, timeout?}] }.
pub(crate) async fn workflows_def_patch(
    State(state): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
    Json(patch): Json<crate::workflow::service::DefFieldsPatch>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    let patched = tokio::task::spawn_blocking(move || svc.edit_definition_fields(&name, &patch))
        .await
        .map_err(internal)?;
    match patched {
        Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(e) => {
            let code = if e.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            Err((code, Json(serde_json::json!({"error": e}))))
        }
    }
}

// ===== Runtime settings (LLM parallelism, retries) =====

/// GET /api/workflows/settings
pub(crate) async fn workflows_settings_get(
    State(state): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    Ok(Json(
        serde_json::to_value(svc.get_settings()).unwrap_or_default(),
    ))
}

/// PUT /api/workflows/settings — persist + apply live.
pub(crate) async fn workflows_settings_put(
    State(state): State<Arc<UiState>>,
    Json(body): Json<crate::workflow::WorkflowSettings>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = service(&state)?;
    match svc.set_settings(body) {
        Ok(applied) => Ok(Json(serde_json::to_value(applied).unwrap_or_default())),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )),
    }
}

fn internal(e: tokio::task::JoinError) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": format!("{e}")})),
    )
}

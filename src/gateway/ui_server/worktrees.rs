//! `/api/worktrees*` — the branches agents worked on in isolation, and the
//! four things a person does with one: read the diff, merge it, rebase it,
//! open a pull request, or throw it away.
//!
//! Every mutating route takes the worktree `path` in the body; the path is
//! validated against the metadata SenClaw wrote when it created the worktree,
//! so an arbitrary directory cannot be merged or removed through here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use super::core::{AppError, UiState};
use crate::worktree;

fn bad(e: impl std::fmt::Display) -> AppError {
    AppError(StatusCode::BAD_REQUEST, e.to_string())
}

/// Only paths SenClaw created (they carry a metadata sidecar) are accepted.
fn known_worktree(path: &str) -> Result<PathBuf, AppError> {
    let p = crate::util::paths::expand_tilde(path.trim());
    if !p.is_absolute() || !p.is_dir() {
        return Err(bad("path must be an existing absolute directory"));
    }
    if worktree::info_for(&p).is_none() {
        return Err(bad("not a SenClaw worktree"));
    }
    Ok(p)
}

#[derive(Deserialize, Default)]
pub(crate) struct ListQuery {
    /// Repository (or any directory inside it).
    repo: String,
    /// Filter by creator, e.g. `kanban:12`, `dispatch:d-4`.
    #[serde(default)]
    owner: Option<String>,
}

/// GET /api/worktrees?repo=&owner=
pub(crate) async fn worktrees_list(
    State(_s): State<Arc<UiState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let repo = crate::util::paths::expand_tilde(q.repo.trim());
    let mut items = tokio::task::spawn_blocking(move || worktree::list(&repo))
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(bad)?;
    if let Some(owner) = q.owner.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
        items.retain(|w| w.owner == owner);
    }
    Ok(Json(serde_json::json!({ "items": items })))
}

#[derive(Deserialize)]
pub(crate) struct PathQuery {
    path: String,
}

/// GET /api/worktrees/diff?path=
pub(crate) async fn worktree_diff(
    State(_s): State<Arc<UiState>>,
    Query(q): Query<PathQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let p = known_worktree(&q.path)?;
    let info = worktree::info_for(&p);
    let d = tokio::task::spawn_blocking(move || worktree::diff(&p))
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(bad)?;
    Ok(Json(serde_json::json!({ "worktree": info, "diff": d })))
}

#[derive(Deserialize)]
pub(crate) struct PathBody {
    path: String,
    #[serde(default)]
    message: Option<String>,
}

/// POST /api/worktrees/merge — merge the branch into the repository's
/// checked-out branch. Refused while the checkout has uncommitted changes.
pub(crate) async fn worktree_merge(
    State(_s): State<Arc<UiState>>,
    Json(body): Json<PathBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let p = known_worktree(&body.path)?;
    let msg = body.message.clone();
    let out = tokio::task::spawn_blocking(move || worktree::merge_into_base(&p, msg.as_deref()))
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(bad)?;
    Ok(Json(serde_json::json!({ "ok": true, "result": out })))
}

/// POST /api/worktrees/rebase — rebase the branch onto its base.
pub(crate) async fn worktree_rebase(
    State(_s): State<Arc<UiState>>,
    Json(body): Json<PathBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let p = known_worktree(&body.path)?;
    let out = tokio::task::spawn_blocking(move || worktree::rebase_onto_base(&p))
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(bad)?;
    Ok(Json(serde_json::json!({ "ok": true, "result": out })))
}

#[derive(Deserialize)]
pub(crate) struct PrBody {
    path: String,
    title: String,
    #[serde(default)]
    body: String,
}

/// POST /api/worktrees/pr — push and `gh pr create`. Without `gh` the error
/// names the branch so the PR can be opened by hand.
pub(crate) async fn worktree_pr(
    State(_s): State<Arc<UiState>>,
    Json(body): Json<PrBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let p = known_worktree(&body.path)?;
    if body.title.trim().is_empty() {
        return Err(bad("title is required"));
    }
    let (title, text) = (body.title.clone(), body.body.clone());
    let url = tokio::task::spawn_blocking(move || worktree::create_pr(&p, &title, &text))
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(bad)?;
    Ok(Json(serde_json::json!({ "ok": true, "url": url })))
}

#[derive(Deserialize)]
pub(crate) struct RemoveBody {
    path: String,
    /// Also delete the `senclaw/…` branch (default true).
    #[serde(default = "yes")]
    delete_branch: bool,
}

fn yes() -> bool {
    true
}

/// POST /api/worktrees/remove
pub(crate) async fn worktree_remove(
    State(_s): State<Arc<UiState>>,
    Json(body): Json<RemoveBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let p = known_worktree(&body.path)?;
    let del = body.delete_branch;
    tokio::task::spawn_blocking(move || worktree::remove(&p, del))
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(bad)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[allow(dead_code)]
fn _path_type_check(_: &Path) {}

//! `/api/datagen/*` — start, watch and cancel dataset-generation jobs, list
//! the datasets on disk, and the catalog and preview behind importing one.
//! The jobs themselves are [`crate::datagen`].
//!
//! Every read and the cancel take an optional `?owner=<chat jid>`: the
//! `senclaw-datagen` MCP server always sends its chat, so a chat's tools reach
//! only that chat's jobs. Without it (the UI, the CLI) everything is visible.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::core::{AppError, UiState};
use crate::datagen::executable_qa::{CodeRunner, SandboxRunner};
use crate::datagen::hf::PublicHf;
use crate::datagen::import::{self, PreviewRequest};
use crate::datagen::jobs::{CancelResult, JobState, StartError};
use crate::datagen::llm::{DaemonLlm, Llm};
use crate::datagen::{catalog, registry, store, Deps, JobSpec, Pipeline};

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OwnerQuery {
    #[serde(default)]
    owner: Option<String>,
}

impl OwnerQuery {
    fn owner(&self) -> Option<&str> {
        self.owner.as_deref().map(str::trim).filter(|o| !o.is_empty())
    }
}

/// POST /api/datagen/jobs — body is a [`JobSpec`]: `pipeline` plus that
/// pipeline's parameters (camelCase; unknown keys are refused), an optional
/// `name`, and the `owner` chat when a chat started it.
pub(crate) async fn datagen_start(
    State(s): State<Arc<UiState>>,
    Json(spec): Json<JobSpec>,
) -> Result<Json<Value>, AppError> {
    let llm: Option<Arc<dyn Llm>> = spec.pipeline.uses_llm().then(|| {
        Arc::new(DaemonLlm {
            config_path: s.config.paths.global_config_path.clone(),
            profile: spec.pipeline.profile().map(str::to_string),
            recorder: s.usage_recorder.clone(),
            usage_label: format!("datagen:{}", spec.name.as_deref().unwrap_or(spec.pipeline.name())),
        }) as Arc<dyn Llm>
    });
    let runner: Option<Arc<dyn CodeRunner>> =
        matches!(spec.pipeline, Pipeline::ExecutableQa(_)).then(|| Arc::new(SandboxRunner) as Arc<dyn CodeRunner>);
    let deps = Deps {
        datasets_root: store::default_root(),
        trajectories_root: crate::trajectory::root_dir(),
        llm,
        runner,
        hf: None,
    };
    let status = registry().start(spec, deps).map_err(|e| {
        let code = match e {
            StartError::Invalid(_) => StatusCode::BAD_REQUEST,
            StartError::Busy(_) => StatusCode::CONFLICT,
            StartError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        AppError(code, e.message().to_string())
    })?;
    Ok(Json(json!({ "ok": true, "job": status })))
}

/// GET /api/datagen/jobs — jobs this daemon started, newest first.
pub(crate) async fn datagen_jobs(State(_s): State<Arc<UiState>>, Query(q): Query<OwnerQuery>) -> Json<Value> {
    Json(json!({ "jobs": registry().list(q.owner()) }))
}

/// GET /api/datagen/jobs/:id
pub(crate) async fn datagen_job(
    State(_s): State<Arc<UiState>>,
    Path(id): Path<String>,
    Query(q): Query<OwnerQuery>,
) -> Result<Json<Value>, AppError> {
    registry()
        .status(&id, q.owner())
        .map(|job| Json(json!({ "job": job })))
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("no datagen job {id} in this daemon run")))
}

/// POST /api/datagen/jobs/:id/cancel — records written so far are kept.
pub(crate) async fn datagen_cancel(
    State(_s): State<Arc<UiState>>,
    Path(id): Path<String>,
    Query(q): Query<OwnerQuery>,
) -> Result<Json<Value>, AppError> {
    match registry().cancel(&id, q.owner()) {
        CancelResult::Cancelled => Ok(Json(json!({ "ok": true, "jobId": id }))),
        CancelResult::NotRunning => Err(AppError(StatusCode::CONFLICT, format!("job {id} is not running"))),
        CancelResult::NotFound => Err(AppError(StatusCode::NOT_FOUND, format!("no datagen job {id} in this daemon run"))),
    }
}

/// GET /api/datagen/datasets — every dataset's manifest, newest first,
/// without its `params` (every seed and inline document of the run). The
/// page polls this while a job runs; the full parameters stay on disk in
/// `manifest.json`.
pub(crate) async fn datagen_datasets(State(_s): State<Arc<UiState>>, Query(q): Query<OwnerQuery>) -> Json<Value> {
    let root = store::default_root();
    let owner = q.owner().map(str::to_string);
    let r = root.clone();
    let datasets = tokio::task::spawn_blocking(move || {
        let mut list = registry().datasets(&r, owner.as_deref());
        for m in &mut list {
            if let Some(o) = m.as_object_mut() {
                o.remove("params");
            }
        }
        list
    })
    .await
    .unwrap_or_default();
    Json(json!({ "root": root.to_string_lossy(), "datasets": datasets }))
}

/// GET /api/datagen/catalog — datasets people wrote, ready to import with
/// `POST /api/datagen/jobs {"pipeline": "import", "source": {"catalog": id}}`,
/// each marked installed when a finished import of it exists.
pub(crate) async fn datagen_catalog(State(_s): State<Arc<UiState>>, Query(q): Query<OwnerQuery>) -> Json<Value> {
    let owner = q.owner().map(str::to_string);
    let datasets = tokio::task::spawn_blocking(move || registry().datasets(&store::default_root(), owner.as_deref()))
        .await
        .unwrap_or_default();
    Json(json!({ "entries": catalog::listing(&datasets) }))
}

/// POST /api/datagen/import/preview — body `{source, kind?, mapping?,
/// labelMap?}`. Reads the first rows and writes nothing: columns, rows, a
/// guessed kind and mapping, and those rows as records (or why each would be
/// dropped).
pub(crate) async fn datagen_import_preview(
    State(_s): State<Arc<UiState>>,
    Json(req): Json<PreviewRequest>,
) -> Result<Json<Value>, AppError> {
    import::preview(req, &PublicHf::new())
        .await
        .map(Json)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e))
}

#[derive(Debug, Deserialize)]
pub(crate) struct RecordsQuery {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

/// The dataset `name`, when it exists and the caller may see it.
fn visible_dataset(name: &str, owner: Option<&str>) -> Result<(std::path::PathBuf, Value), AppError> {
    let not_found = || AppError(StatusCode::NOT_FOUND, format!("no dataset {name}"));
    let dir = store::dataset_dir(&store::default_root(), name).ok_or_else(not_found)?;
    let manifest = store::read_manifest(&dir).ok_or_else(not_found)?;
    if owner.is_some_and(|o| manifest.get("owner").and_then(Value::as_str) != Some(o)) {
        return Err(not_found());
    }
    Ok((dir, manifest))
}

/// Largest records page; an exported turn can be tens of KB.
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

/// GET /api/datagen/datasets/:name/records?offset=&limit= — a page of records
/// (at most 200, at most 2 MB) and the total.
pub(crate) async fn datagen_records(
    State(_s): State<Arc<UiState>>,
    Path(name): Path<String>,
    Query(q): Query<RecordsQuery>,
) -> Result<Json<Value>, AppError> {
    let owner = q.owner.as_deref().map(str::trim).filter(|o| !o.is_empty());
    let (dir, _) = visible_dataset(&name, owner)?;
    let (offset, limit) = (q.offset.unwrap_or(0), q.limit.unwrap_or(20).clamp(1, 200));
    let (records, total) = tokio::task::spawn_blocking(move || store::read_records(&dir, offset, limit, MAX_PAGE_BYTES))
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "name": name, "offset": offset, "total": total, "records": records })))
}

/// DELETE /api/datagen/datasets/:name — refused while its job is running.
pub(crate) async fn datagen_delete_dataset(
    State(_s): State<Arc<UiState>>,
    Path(name): Path<String>,
    Query(q): Query<OwnerQuery>,
) -> Result<Json<Value>, AppError> {
    let (dir, manifest) = visible_dataset(&name, q.owner())?;
    let job = manifest.get("jobId").and_then(Value::as_str).unwrap_or_default();
    if registry().status(job, None).is_some_and(|s| s.state == JobState::Running) {
        return Err(AppError(StatusCode::CONFLICT, format!("dataset {name} is still being written — cancel its job first")));
    }
    tokio::task::spawn_blocking(move || store::delete(&dir))
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "ok": true, "name": name })))
}

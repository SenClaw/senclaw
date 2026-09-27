//! `/api/local-models/*` (`docs/runtime-protocol.md` §5.3) and the
//! `local:<key>` LLM-config bridge `load_llm_configs` merges in.

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use sen_runtime_sdk::manifest::Capability;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::gateway::group_manager::LlmConfig;
use crate::gateway::ui_server::core::{AppError, UiState};
use crate::runtime::manager::RuntimeManager;
use crate::runtime::proxy::error_response;
use crate::runtime::store::InstalledPackage;

use super::settings::resolve_context_length;
use super::{config_id, download, hf_files, scan, settings};

fn internal(e: impl std::fmt::Display) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad(msg: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.into())
}

/// Every chat-capable local model as a `local:<key>` LLM config — appended by
/// `load_llm_configs` (§5.4). Reached through the daemon's own model route, so
/// its `apiKey` is empty by design (same rule as an app-provided config).
pub fn llm_configs(local_models_dir: &Path) -> Vec<LlmConfig> {
    let port = crate::util::internal_auth::daemon_ui_port();
    let default_context_length = settings::load_daemon_settings(local_models_dir).default_context_length;
    scan::scan_all(local_models_dir)
        .into_iter()
        .filter(|m| m.capabilities.contains(&Capability::Chat))
        .map(|m| LlmConfig {
            id: config_id(&m.key),
            label: format!("Local · {}", m.name),
            provider: "local".to_string(),
            base_url: format!("http://127.0.0.1:{port}/api/runtimes/models/{}/v1", m.key),
            api_key: String::new(),
            model_name: m.key.clone(),
            adapt: "openai".to_string(),
            max_tokens: 8192,
            // No live request to defer to here — always the daemon's effective
            // cap (§5.3), so the picker's advertised budget matches what a
            // JIT load through the model route will actually launch with.
            context_length: resolve_context_length(None, m.context_length, default_context_length),
            vision: Some(m.vision),
            edit_format: None,
            auth: None,
            oauth_account_id: None,
        })
        .collect()
}

/// `installed`, when given, is an already-scanned runtime list — a caller
/// rendering several models in one response (`GET /api/local-models`) scans
/// it once up front instead of paying for a fresh runtimes-directory scan per
/// model (`manifest_for_slot` would otherwise do exactly that inside this
/// function, once per `LocalModel`).
fn model_view(mgr: Option<&Arc<RuntimeManager>>, installed: Option<&[InstalledPackage]>, m: &scan::LocalModel) -> Value {
    let key_for_process = format!("model:{}", m.key);
    let process = mgr.and_then(|mgr| mgr.process(&key_for_process)).map(|p| p.view(Some(m.slot().as_str()), Some(&m.key)));
    let selected = mgr.and_then(|mgr| match installed {
        Some(installed) => mgr.manifest_for_slot_from(m.slot(), installed),
        None => mgr.manifest_for_slot(m.slot()),
    });
    let selected = selected.map(|p| json!({"id": p.manifest.id, "version": p.manifest.version, "name": p.manifest.name}));
    json!({
        "key": m.key, "name": m.name, "format": m.format, "path": m.path, "sizeBytes": m.size_bytes,
        "capabilities": m.capabilities, "vision": m.vision, "embedding": m.embedding,
        "mmprojPath": m.mmproj_path, "quant": m.quant, "repo": m.repo, "contextLength": m.context_length,
        "runtime": {"slot": m.slot().as_str(), "selected": selected},
        "process": process,
    })
}

pub(crate) async fn get_local_models(State(s): State<Arc<UiState>>) -> Result<Json<Value>, AppError> {
    let root = s.config.paths.local_models_dir.clone();
    let root_for_scan = root.clone();
    let models = tokio::task::spawn_blocking(move || scan::scan_all(&root_for_scan)).await.map_err(internal)?;
    // Scanned once up front — see `model_view`'s doc comment.
    let installed = s.runtime_manager.as_ref().map(|mgr| mgr.installed());
    let views: Vec<Value> =
        models.iter().map(|m| model_view(s.runtime_manager.as_ref(), installed.as_deref(), m)).collect();
    Ok(Json(json!({"root": root, "models": views, "downloads": download::all()})))
}

#[derive(Deserialize)]
pub(crate) struct HfFilesQuery {
    repo: String,
    #[serde(default)]
    revision: Option<String>,
}

pub(crate) async fn get_hf_files(Query(q): Query<HfFilesQuery>) -> Result<Json<Value>, AppError> {
    let repo = hf_files::normalize_repo(&q.repo).map_err(bad)?;
    let revision = q.revision.as_deref().unwrap_or("main");
    let resp = hf_files::fetch(&repo, revision).await.map_err(|e| bad(format!("{e:#}")))?;
    Ok(Json(serde_json::to_value(resp).map_err(internal)?))
}

#[derive(Deserialize)]
pub(crate) struct DownloadBody {
    repo: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    mmproj: Option<String>,
    #[serde(default)]
    revision: Option<String>,
}

pub(crate) async fn post_download(State(s): State<Arc<UiState>>, Json(body): Json<DownloadBody>) -> Result<Response, AppError> {
    let repo = hf_files::normalize_repo(&body.repo).map_err(bad)?;
    let revision = body.revision.as_deref().unwrap_or("main").to_string();
    let format = if body.file.is_some() { "gguf" } else { "mlx" };
    if format == "gguf" && body.file.is_none() {
        return Err(bad("a GGUF download needs `file`"));
    }
    let state = download::start(&s.config.paths.local_models_dir, &repo, format, &revision, body.file, body.mmproj);
    Ok((StatusCode::ACCEPTED, Json(json!({"downloadId": state.download_id}))).into_response())
}

pub(crate) async fn get_downloads() -> Json<Value> {
    Json(json!({"downloads": download::all()}))
}

pub(crate) async fn get_download(AxumPath(id): AxumPath<String>) -> Result<Json<Value>, AppError> {
    let state = download::status(&id).ok_or_else(|| AppError(StatusCode::NOT_FOUND, "no such download".into()))?;
    Ok(Json(serde_json::to_value(state).map_err(internal)?))
}

pub(crate) async fn post_download_cancel(AxumPath(id): AxumPath<String>) -> Json<Value> {
    Json(json!({"ok": true, "cancelled": download::cancel(&id)}))
}

#[derive(Deserialize)]
pub(crate) struct DeleteQuery {
    #[serde(default)]
    force: Option<i32>,
}

pub(crate) async fn delete_model(
    State(s): State<Arc<UiState>>,
    AxumPath(key): AxumPath<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<Json<Value>, AppError> {
    let dir = s.config.paths.local_models_dir.clone();
    let all_models = tokio::task::spawn_blocking(move || scan::scan_all(&dir)).await.map_err(internal)?;
    let model = all_models
        .iter()
        .find(|m| m.key == key)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("no local model with key `{key}`")))?;
    let process_key = format!("model:{key}");
    let loaded = s.runtime_manager.as_ref().is_some_and(|mgr| mgr.process(&process_key).is_some());
    if loaded && q.force.unwrap_or(0) == 0 {
        return Err(AppError(StatusCode::CONFLICT, "the model is loaded — pass force=1 to unload and delete it".into()));
    }
    if loaded {
        if let Some(mgr) = &s.runtime_manager {
            mgr.stop_process(&process_key).await;
        }
    }
    let remove_path = |p: &Path| -> std::io::Result<()> {
        if p.is_dir() {
            std::fs::remove_dir_all(p)
        } else if p.exists() {
            std::fs::remove_file(p)
        } else {
            Ok(())
        }
    };
    remove_path(&model.path).map_err(internal)?;
    if let Some(mmproj) = &model.mmproj_path {
        // A `mmproj-*.gguf` pairs with every quant in its repo directory, not
        // just this one (`docs/runtime-protocol.md` §5.3: "deleting a GGUF
        // file keeps a mmproj-*.gguf that another model in the same folder
        // still uses").
        let still_used = all_models.iter().any(|other| other.key != model.key && other.mmproj_path.as_deref() == Some(mmproj.as_path()));
        if !still_used {
            remove_path(mmproj).map_err(internal)?;
        }
    }
    Ok(Json(json!({"ok": true})))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoadBody {
    #[serde(default)]
    context_length: Option<u32>,
}

/// `POST /api/local-models/:key/load` — the body is **optional** (none, `{}`,
/// or `{"contextLength": …}`); a bare `Json<LoadBody>` extractor 415s a
/// request with no body or no `Content-Type`, which is exactly what both
/// clients send when the caller has no explicit context length to pin
/// (`docs/runtime-protocol.md` §5.3).
pub(crate) async fn post_load(State(s): State<Arc<UiState>>, AxumPath(key): AxumPath<String>, body: axum::body::Bytes) -> Response {
    let body: LoadBody = if body.is_empty() {
        LoadBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(b) => b,
            Err(e) => return AppError(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")).into_response(),
        }
    };
    let Some(model) = scan::find_by_key(&s.config.paths.local_models_dir, &key) else {
        return AppError(StatusCode::NOT_FOUND, format!("no local model with key `{key}`")).into_response();
    };
    let slot = model.slot();
    let Some(manager) = s.runtime_manager.as_ref() else {
        return AppError(StatusCode::INTERNAL_SERVER_ERROR, "the runtime manager is not wired".into()).into_response();
    };
    let default_context_length = settings::load_daemon_settings(&s.config.paths.local_models_dir).default_context_length;
    let context_length = resolve_context_length(body.context_length, model.context_length, default_context_length);
    let dial = match manager
        .ensure_model_started(model.format, &model.key, &model.path, model.mmproj_path.as_deref(), context_length, model.capability_list())
        .await
    {
        Ok(d) => d,
        // The same structured body the legacy-namespace proxy answers with
        // (`code`, `slot`, `error`) — a failure here must not look like a
        // different kind of problem than the same runtime failing to start
        // for a chat request.
        Err(e) => return error_response(e, slot),
    };
    let Some(proc) = manager.process(&dial.process_key) else {
        return AppError(StatusCode::INTERNAL_SERVER_ERROR, "started but not tracked".into()).into_response();
    };
    Json(proc.view(Some(slot.as_str()), Some(&model.key))).into_response()
}

pub(crate) async fn post_unload(State(s): State<Arc<UiState>>, AxumPath(key): AxumPath<String>) -> Result<Json<Value>, AppError> {
    let manager = s
        .runtime_manager
        .as_ref()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "the runtime manager is not wired".into()))?;
    let stopped = manager.stop_process(&format!("model:{key}")).await;
    Ok(Json(json!({"ok": true, "unloaded": stopped})))
}

pub(crate) async fn get_settings(State(s): State<Arc<UiState>>) -> Result<Json<Value>, AppError> {
    let daemon = settings::load_daemon_settings(&s.config.paths.local_models_dir);
    let engine = settings::load_engine_settings(&s.config.paths.local_models_dir);
    Ok(Json(json!({"defaultContextLength": daemon.default_context_length, "engine": engine})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettingsBody {
    #[serde(default)]
    default_context_length: Option<u32>,
    #[serde(default)]
    engine: Option<Value>,
}

pub(crate) async fn put_settings(State(s): State<Arc<UiState>>, Json(body): Json<SettingsBody>) -> Result<Json<Value>, AppError> {
    if body.default_context_length.is_some() {
        settings::save_daemon_settings(
            &s.config.paths.local_models_dir,
            &settings::DaemonModelSettings { default_context_length: body.default_context_length },
        )
        .map_err(internal)?;
    }
    if let Some(engine) = &body.engine {
        settings::save_engine_settings(&s.config.paths.local_models_dir, engine).map_err(internal)?;
    }
    get_settings(State(s)).await
}

//! `/api/runtimes/*` — runtime management REST (`docs/runtime-protocol.md` §5.1).

use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use sen_runtime_sdk::manifest::{RuntimeManifest, Slot};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::gateway::ui_server::core::{AppError, UiState};

use super::index::CachedIndex;
use super::manager::RuntimeManager;
use super::proxy::error_response;
use super::version::cmp_versions;

fn manager(s: &UiState) -> Result<&Arc<RuntimeManager>, AppError> {
    s.runtime_manager
        .as_ref()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "the runtime manager is not wired".into()))
}

fn internal(e: impl std::fmt::Display) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad(msg: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.into())
}

fn installed_view(pkg: &super::store::InstalledPackage, all_versions: &[String], compatible: bool) -> Value {
    let m: &RuntimeManifest = &pkg.manifest;
    json!({
        "id": m.id, "name": m.name, "version": m.version, "versions": all_versions,
        "type": m.runtime_type, "slots": m.slots, "formats": m.formats, "capabilities": m.capabilities,
        "platforms": m.platforms, "accelerator": m.accelerator, "mode": m.mode, "compatible": compatible,
        "source": pkg.source, "description": m.description, "releaseNotesUrl": m.release_notes_url,
        "warnings": pkg.warnings,
    })
}

pub(crate) async fn get_runtimes(State(s): State<Arc<UiState>>) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    let platform = sen_runtime_sdk::platform::current();
    let installed = manager.installed();

    // Group by id -> newest-first versions, one row per id (the newest
    // installed version is what `installed[].version` reports).
    let mut by_id: std::collections::BTreeMap<String, Vec<&super::store::InstalledPackage>> = Default::default();
    for pkg in &installed {
        by_id.entry(pkg.manifest.id.clone()).or_default().push(pkg);
    }
    let mut installed_view_list = Vec::new();
    for (_id, mut pkgs) in by_id {
        pkgs.sort_by(|a, b| cmp_versions(&b.manifest.version, &a.manifest.version));
        let versions: Vec<String> = pkgs.iter().map(|p| p.manifest.version.clone()).collect();
        let compatible = pkgs[0].manifest.supports_platform(platform);
        installed_view_list.push(installed_view(pkgs[0], &versions, compatible));
    }

    let settings = manager.settings();
    let mut slots = Vec::new();
    for slot in Slot::ALL {
        let candidates: Vec<Value> = installed
            .iter()
            .filter(|p| p.manifest.slots.contains(&slot) && p.manifest.supports_platform(platform))
            .map(|p| json!({"id": p.manifest.id, "version": p.manifest.version, "name": p.manifest.name}))
            .collect();
        let selected = manager.manifest_for_slot_from(slot, &installed).map(|p| {
            json!({"id": p.manifest.id, "version": p.manifest.version, "name": p.manifest.name})
        });
        let kind = if slot.format().is_some() { "format" } else { "capability" };
        slots.push(json!({
            "slot": slot.as_str(), "label": slot.label(), "kind": kind,
            "selected": selected, "candidates": candidates,
        }));
    }

    // Scanned once for every `model:`-keyed process below, instead of a fresh
    // directory + GGUF-header scan per process.
    let local_models = crate::local_models::scan_all(&s.config.paths.local_models_dir);
    let processes: Vec<Value> = manager
        .processes()
        .iter()
        .map(|p| {
            let snap_key = p.snapshot().key;
            let (slot, model_key): (Option<&str>, Option<&str>) = if let Some(key) = snap_key.strip_prefix("model:") {
                let slot = local_models.iter().find(|m| m.key == key).map(|m| m.slot().as_str());
                (slot, Some(key))
            } else {
                // A service key is `service:<runtime-id>` — its slot is
                // whatever this installed runtime declares (a service-mode
                // manifest names exactly one).
                let slot = installed
                    .iter()
                    .find(|pkg| pkg.manifest.id == p.runtime_id)
                    .and_then(|pkg| pkg.manifest.slots.first())
                    .map(|s| s.as_str());
                (slot, None)
            };
            p.view(slot, model_key)
        })
        .collect();

    Ok(Json(json!({
        "platform": platform,
        "settings": {
            "autoUpdate": settings.auto_update, "channel": settings.channel,
            "idleTimeoutSecs": {"service": settings.idle_timeout_secs.service, "model": settings.idle_timeout_secs.model},
        },
        "slots": slots,
        "installed": installed_view_list,
        "processes": processes,
    })))
}

#[derive(Deserialize)]
pub(crate) struct CatalogQuery {
    #[serde(default)]
    refresh: Option<i32>,
}

pub(crate) async fn get_catalog(State(s): State<Arc<UiState>>, Query(q): Query<CatalogQuery>) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    let (cached, error): (CachedIndex, Option<String>) = if q.refresh.unwrap_or(0) != 0 || manager.cached_index().is_none() {
        manager.refresh_index_with_error().await
    } else {
        (manager.cached_index().expect("checked is_none above"), None)
    };
    let settings = manager.settings();
    let platform = sen_runtime_sdk::platform::current();
    let installed = manager.installed();
    let entries: Vec<Value> = cached
        .index
        .runtimes
        .iter()
        .map(|e| {
            let compatible = e.platforms.iter().any(|p| p == platform);
            let latest_version = cached.effective_channel_version(e, settings.channel);
            let latest_release = latest_version.as_deref().and_then(|v| e.release(v));
            let package = latest_release.and_then(|r| e.package_for(r, platform));
            // §5.1: "available" = compatible AND a package exists for this
            // platform on this channel — for an `upstream` entry (no
            // per-release package list here), an asset mapped for this
            // platform counts instead of a `Release`/`PackageAsset` existing.
            let available = compatible
                && match &e.upstream {
                    Some(u) => u.assets.contains_key(platform),
                    None => package.is_some(),
                };
            let installed_version = installed
                .iter()
                .filter(|p| p.manifest.id == e.id)
                .map(|p| p.manifest.version.clone())
                .max_by(|a, b| cmp_versions(a, b));
            let update_available = match (&installed_version, &latest_version) {
                (Some(cur), Some(latest)) => cmp_versions(cur, latest) == std::cmp::Ordering::Less,
                // Includes `"latest"` still unresolved — never claim an
                // update is ready before there is a real version to name.
                _ => false,
            };
            let release_notes_url = latest_release
                .and_then(|r| r.notes_url.clone())
                .or_else(|| match (&e.upstream, &latest_version) {
                    (Some(u), Some(tag)) => Some(format!("https://github.com/{}/releases/tag/{tag}", u.repo)),
                    _ => None,
                });
            let download_size = package.and_then(|p| p.size);
            json!({
                "id": e.id, "name": e.name, "description": e.description, "type": e.runtime_type,
                "slots": e.slots, "formats": e.formats, "capabilities": e.capabilities,
                "accelerator": e.accelerator, "platforms": e.platforms, "compatible": compatible,
                "available": available, "latestVersion": latest_version, "installedVersion": installed_version,
                "updateAvailable": update_available, "releaseNotesUrl": release_notes_url, "downloadSize": download_size,
            })
        })
        .collect();
    Ok(Json(json!({
        "channel": settings.channel, "fetchedAt": cached.fetched_at, "source": cached.source,
        "error": error, "entries": entries,
    })))
}

#[derive(Deserialize)]
pub(crate) struct InstallBody {
    id: String,
    #[serde(default)]
    version: Option<String>,
}

pub(crate) async fn post_install(State(s): State<Arc<UiState>>, Json(body): Json<InstallBody>) -> Result<Response, AppError> {
    let manager = manager(&s)?;
    let job = manager.start_install(&body.id, body.version).map_err(|e| bad(e.to_string()))?;
    Ok((StatusCode::ACCEPTED, Json(json!({"jobId": job.job_id, "id": job.id, "version": job.version}))).into_response())
}

#[derive(Deserialize)]
pub(crate) struct InstallLocalBody {
    path: String,
}

pub(crate) async fn post_install_local(
    State(s): State<Arc<UiState>>,
    Json(body): Json<InstallLocalBody>,
) -> Result<Json<Value>, AppError> {
    let manager = Arc::clone(manager(&s)?);
    let path = crate::util::paths::expand_tilde(body.path.trim());
    // `install_local` copies or extracts synchronously (a multi-hundred-MB
    // llama.cpp archive) — off the async worker.
    let pkg = tokio::task::spawn_blocking(move || manager.install_local(&path))
        .await
        .map_err(internal)?
        .map_err(|e| bad(e.to_string()))?;
    let platform = sen_runtime_sdk::platform::current();
    let compatible = pkg.manifest.supports_platform(platform);
    Ok(Json(installed_view(&pkg, &[pkg.manifest.version.clone()], compatible)))
}

pub(crate) async fn get_jobs(State(s): State<Arc<UiState>>) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    Ok(Json(json!({ "jobs": manager.jobs.list() })))
}

pub(crate) async fn get_job(State(s): State<Arc<UiState>>, AxumPath(job_id): AxumPath<String>) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    let job = manager.jobs.status(&job_id).ok_or_else(|| AppError(StatusCode::NOT_FOUND, "no such job".into()))?;
    Ok(Json(serde_json::to_value(job).map_err(internal)?))
}

pub(crate) async fn post_job_cancel(State(s): State<Arc<UiState>>, AxumPath(job_id): AxumPath<String>) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    Ok(Json(json!({"ok": true, "cancelled": manager.jobs.cancel(&job_id)})))
}

#[derive(Deserialize)]
pub(crate) struct UninstallQuery {
    #[serde(default)]
    force: Option<i32>,
}

pub(crate) async fn delete_version(
    State(s): State<Arc<UiState>>,
    AxumPath((id, version)): AxumPath<(String, String)>,
    Query(q): Query<UninstallQuery>,
) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    let force = q.force.unwrap_or(0) != 0;
    manager
        .uninstall(&id, &version, force)
        .await
        .map_err(|e| AppError(StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub(crate) struct SelectionBody {
    slot: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

pub(crate) async fn put_selections(State(s): State<Arc<UiState>>, Json(body): Json<SelectionBody>) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    let slot = Slot::parse(&body.slot).ok_or_else(|| bad(format!("`{}` is not a slot", body.slot)))?;
    manager.select(slot, body.id, body.version).map_err(|e| bad(e.to_string()))?;
    get_runtimes(State(s)).await
}

pub(crate) async fn get_settings(State(s): State<Arc<UiState>>) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    let settings = manager.settings();
    Ok(Json(json!({
        "autoUpdate": settings.auto_update, "channel": settings.channel,
        "idleTimeoutSecs": {"service": settings.idle_timeout_secs.service, "model": settings.idle_timeout_secs.model},
    })))
}

/// `{service?, model?}` — each independently optional, so `PUT
/// /api/runtimes/settings` can move just one of the two without a caller
/// having to first read the other back (`docs/runtime-protocol.md` §5.1: "a
/// partial merge of camelCase fields").
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PartialIdleTimeouts {
    #[serde(default)]
    service: Option<u64>,
    #[serde(default)]
    model: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettingsBody {
    #[serde(default)]
    auto_update: Option<bool>,
    #[serde(default)]
    channel: Option<super::settings::Channel>,
    #[serde(default)]
    idle_timeout_secs: Option<PartialIdleTimeouts>,
}

pub(crate) async fn put_settings(State(s): State<Arc<UiState>>, Json(body): Json<SettingsBody>) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    let mut settings = manager.settings();
    if let Some(v) = body.auto_update {
        settings.auto_update = v;
    }
    if let Some(v) = body.channel {
        settings.channel = v;
    }
    if let Some(v) = body.idle_timeout_secs {
        if let Some(service) = v.service {
            settings.idle_timeout_secs.service = service;
        }
        if let Some(model) = v.model {
            settings.idle_timeout_secs.model = model;
        }
    }
    manager.replace_settings(settings).map_err(internal)?;
    get_settings(State(s)).await
}

pub(crate) async fn post_slot_start(State(s): State<Arc<UiState>>, AxumPath(slot): AxumPath<String>) -> Result<Response, AppError> {
    let manager = manager(&s)?;
    let Some(parsed_slot) = Slot::parse(&slot) else {
        return Err(bad(format!("`{slot}` is not a slot")));
    };
    let dial = match manager.ensure_slot_started(parsed_slot).await {
        Ok(d) => d,
        // The §5.2 structured body, same as the proxy this warms up for —
        // this is the only route that started a service and reported its
        // failure differently from every other one.
        Err(e) => return Ok(error_response(e, parsed_slot)),
    };
    let proc = manager.process(&dial.process_key).ok_or_else(|| internal("started but not tracked"))?;
    Ok(Json(proc.view(Some(parsed_slot.as_str()), None)).into_response())
}

pub(crate) async fn post_process_stop(State(s): State<Arc<UiState>>, AxumPath(key): AxumPath<String>) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    let key = urlencoding::decode(&key).map(|c| c.into_owned()).unwrap_or(key);
    Ok(Json(json!({"ok": true, "stopped": manager.stop_process(&key).await})))
}

#[derive(Deserialize)]
pub(crate) struct LogsQuery {
    #[serde(default)]
    lines: Option<usize>,
    /// A specific process key (`service:<id>` or `model:<model-key>`) — reads
    /// that process's own log. Without it: the service log if there is one,
    /// else the runtime's most recently modified model log
    /// (`docs/runtime-protocol.md` §5.1).
    #[serde(default)]
    key: Option<String>,
}

/// `:id` and a `model:`-prefixed `?key=` are interpolated straight into a
/// log filename (`RuntimeManager::resolve_log_path`) — validated here, at the
/// HTTP boundary, before either reaches it. A non-`model:` key is not
/// checked: `resolve_log_path` never uses it (it falls back to the service
/// log, keyed only on `id`), so it cannot reach the vulnerable interpolation.
fn validate_logs_params(id: &str, key: Option<&str>) -> Result<(), AppError> {
    if !sen_runtime_sdk::manifest::valid_id(id) {
        return Err(bad(format!("`{id}` is not a valid runtime id")));
    }
    if let Some(model_key) = key.and_then(|k| k.strip_prefix("model:")) {
        if model_key.contains('/') || model_key.contains('\\') || model_key.contains("..") {
            return Err(bad("`key`'s `model:` suffix must not contain a path separator or `..`"));
        }
    }
    Ok(())
}

pub(crate) async fn get_logs(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<LogsQuery>,
) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    validate_logs_params(&id, q.key.as_deref())?;
    let (path, lines) = manager.logs(&id, q.lines.unwrap_or(200).min(5000), q.key.as_deref()).map_err(internal)?;
    Ok(Json(json!({"path": path, "lines": lines})))
}

pub(crate) async fn post_check_updates(State(s): State<Arc<UiState>>) -> Result<Json<Value>, AppError> {
    let manager = manager(&s)?;
    let (cached, error) = manager.refresh_index_with_error().await;
    let (updates, started) = super::updates::check_updates(manager, &cached).await;
    let channel = manager.settings().channel;
    Ok(Json(json!({
        "checkedAt": cached.fetched_at, "channel": channel, "updates": updates, "started": started, "error": error,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GET /api/runtimes/:id/logs` interpolates `id` and a `model:`-
    /// prefixed `key` straight into a log filename — both must be rejected
    /// before they ever reach it.
    #[test]
    fn logs_params_reject_a_traversal_id_or_model_key() {
        assert!(validate_logs_params("llama.cpp-metal", None).is_ok());
        assert!(validate_logs_params("llama.cpp-metal", Some("model:my-model")).is_ok());
        assert!(
            validate_logs_params("llama.cpp-metal", Some("service:sen-ocr")).is_ok(),
            "a non-`model:` key never reaches the vulnerable interpolation"
        );

        assert!(validate_logs_params("../../etc", None).is_err(), "id must be validated");
        assert!(validate_logs_params("/etc/passwd", None).is_err());
        assert!(validate_logs_params("", None).is_err());

        assert!(validate_logs_params("llama.cpp-metal", Some("model:../../../secrets")).is_err());
        assert!(validate_logs_params("llama.cpp-metal", Some("model:sub/dir")).is_err());
        assert!(validate_logs_params("llama.cpp-metal", Some("model:sub\\dir")).is_err());
    }
}

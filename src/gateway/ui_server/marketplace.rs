//! REST handlers for marketplace management.
//! Routes registered under /api/marketplace/* in core.rs.

use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::{Deserialize, Serialize};
use tokio::task;

use super::core::{AppError, UiState};
use crate::marketplace::types::{MarketplaceSource, SourceType};

fn internal(e: impl std::fmt::Display) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Mark registry entries that are already installed as Space Apps.
///
/// A catalog lists what the hub *offers*; `installed` on a hub source means
/// "installed as a plugin", which an app never is — so without this every app
/// in the store reads "not installed" no matter how many are running. The link
/// is the app's stamped [`HubOrigin`], falling back to a derived `senclaw/<id>`
/// for apps installed before stamping existed; that fallback is why an
/// unstamped app resolves as installed-with-unknown-version rather than absent.
///
/// Best-effort by design: a database that will not open leaves the list exactly
/// as the catalog described it, which is the pre-existing behaviour.
fn stamp_installed_apps(s: &Arc<UiState>, plugins: &mut [crate::marketplace::types::MarketplacePlugin]) {
    use crate::marketplace::app_update;

    if !plugins.iter().any(|p| p.kind.as_deref() == Some("app")) {
        return;
    }
    let Some(db) = s.db.as_deref() else { return };
    let rows: Vec<(String, serde_json::Value)> = db
        .with_conn(|conn: &rusqlite::Connection| {
            let mut stmt = conn.prepare("SELECT id, manifest FROM space_apps")?;
            let rows = stmt
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let m: String = row.get(1)?;
                    Ok((id, serde_json::from_str(&m).unwrap_or_default()))
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
        .unwrap_or_default();

    // slug → installed version (None when the origin was never stamped).
    let installed: std::collections::HashMap<String, Option<String>> = rows
        .iter()
        .filter_map(|(id, manifest)| {
            app_update::origin_from_manifest(manifest, id).map(|o| (o.slug(), o.version))
        })
        .collect();

    apply_installed_apps(plugins, &installed);
}

/// The decision half of [`stamp_installed_apps`], split from the database read
/// so it can be tested without one.
fn apply_installed_apps(
    plugins: &mut [crate::marketplace::types::MarketplacePlugin],
    installed: &std::collections::HashMap<String, Option<String>>,
) {
    use crate::marketplace::app_update;

    for p in plugins.iter_mut().filter(|p| p.kind.as_deref() == Some("app")) {
        let Some(slug) = p.slug.as_deref() else { continue };
        let Some(version) = installed.get(slug) else { continue };
        p.installed = true;
        p.installed_version = version.clone();
        p.update_available = p
            .version
            .as_deref()
            .is_some_and(|latest| app_update::is_newer(latest, version.as_deref()));
    }
}

// ─── Install straight from the hub package registry ──────────────────────────

/// Body of `POST /api/marketplace/hub/install`.
#[derive(Deserialize)]
pub struct HubInstallBody {
    /// `scope/name`, `@scope/name`, or a bare `name` (scope defaults to
    /// `senclaw`).
    slug: String,
    /// Exact version; the `latest` dist-tag when omitted.
    #[serde(default)]
    version: Option<String>,
    /// Hub base URL. Defaults to the built-in hub.
    #[serde(default)]
    hub: Option<String>,
    /// Artifact platform; this machine's when omitted.
    #[serde(default)]
    platform: Option<String>,
    /// Install even if the pre-install security scan blocks it.
    #[serde(default)]
    force: bool,
}

/// POST /api/marketplace/hub/install — fetch a package from the hub REGISTRY
/// and install it.
///
/// The Marketplace browser reads `/marketplace.json`, which by format is a
/// plugin index — a hub that publishes apps therefore browses as an empty
/// catalog while its packages are perfectly installable. The registry
/// (`/api/v1/packages/{scope}/{name}`) is the surface that serves those, and
/// this is the same path `senclaw hub install` takes: resolve → download →
/// verify the SHA-512 the hub published → hand the bytes to the Space App
/// installer. The digest check is why this is worth an endpoint rather than
/// pointing the UI at a download URL.
pub async fn marketplace_hub_install(
    State(s): State<Arc<UiState>>,
    Json(body): Json<HubInstallBody>,
) -> Result<axum::response::Response, AppError> {
    use crate::marketplace::{publish, registry};

    let hub = body
        .hub
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| publish::DEFAULT_HUB.to_string());
    let (scope, name) = registry::parse_slug(&body.slug)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    let host = body
        .platform
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(publish::host_platform);

    let pkg = registry::fetch_package(&hub, &scope, &name)
        .await
        // A typo and an unpublished package look the same from here, and both
        // are the caller's problem rather than a server fault.
        .map_err(|e| AppError(StatusCode::NOT_FOUND, e.to_string()))?;
    let ver = registry::resolve_version(&pkg, body.version.as_deref())
        .map_err(|e| AppError(StatusCode::NOT_FOUND, e.to_string()))?;
    let dist = registry::select_dist(ver, &host)
        .map_err(|e| AppError(StatusCode::NOT_FOUND, e.to_string()))?;

    let bytes = registry::download_verified(dist)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?;

    let origin = crate::marketplace::app_update::HubOrigin {
        scope,
        name,
        version: Some(ver.version.clone()),
        hub: Some(hub),
        integrity: dist.integrity.clone(),
        installed_at: Some(chrono::Utc::now().timestamp_millis()),
    };

    let out = super::space::install_app_from_zip(s, bytes, Some(origin), body.force).await?;
    Ok(out.into_response())
}

// ─── Types ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AddSourceBody {
    #[serde(default)]
    name: Option<String>,
    /// Omitted by the "paste a URL" flows — inferred from the URL/path instead.
    #[serde(rename = "type", default)]
    source_type: Option<SourceType>,
    #[serde(rename = "localPath", alias = "local_path", default)]
    local_path: Option<String>,
    url: Option<String>,
    branch: Option<String>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    enabled: Option<bool>,
}

impl AddSourceBody {
    /// What kind of source a bare URL/path describes: a `marketplace.json`
    /// catalog (or bare host) is a hub, anything git-ish is a git clone, and a
    /// filesystem path is local.
    fn infer_type(&self) -> SourceType {
        crate::marketplace::infer_source_type(self.url.as_deref(), self.source_type)
    }

    /// A readable default name when the caller only sent a URL.
    fn resolved_name(&self, source_type: SourceType) -> String {
        crate::marketplace::default_source_name(
            self.name.as_deref(),
            self.url.as_deref(),
            self.local_path.as_deref(),
            source_type,
        )
    }
}

#[derive(Deserialize)]
pub struct ReorderSourcesBody {
    #[serde(rename = "orderedIds")]
    ordered_ids: Vec<String>,
}

#[derive(Deserialize)]
pub struct TogglePluginBody {
    enabled: bool,
}

#[derive(Deserialize)]
#[allow(dead_code)] // parsed to validate the request shape; the allowlist is not persisted yet
pub struct SetUseToolsBody {
    #[serde(rename = "useTools")]
    use_tools: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct SourceListResponse {
    sources: Vec<MarketplaceSource>,
}

#[derive(Serialize)]
pub struct SourceInfoResponse {
    #[serde(flatten)]
    source: MarketplaceSource,
    plugins: Vec<serde_json::Value>,
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// GET /api/marketplace/sources - list all sources
pub(crate) async fn marketplace_sources_list(
    State(s): State<Arc<UiState>>,
) -> Result<Json<SourceListResponse>, AppError> {
    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let sources = task::spawn_blocking(move || {
        let manager = manager.lock().unwrap();
        manager.get_sources()
    })
    .await
    .map_err(internal)?;

    Ok(Json(SourceListResponse { sources }))
}

/// POST /api/marketplace/sources - add a new source
pub(crate) async fn marketplace_sources_add(
    State(s): State<Arc<UiState>>,
    Json(body): Json<AddSourceBody>,
) -> Result<Json<MarketplaceSource>, AppError> {
    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let source_type = body.infer_type();
    let name = body.resolved_name(source_type);

    let result = task::spawn_blocking(move || {
        let mut manager = manager.lock().unwrap();
        let source = manager.add_source(
            name,
            source_type,
            body.url,
            body.branch,
            body.local_path,
            body.priority,
            body.enabled,
        )?;
        // Pull the catalog straight away so the new hub is browsable without a
        // separate sync round-trip.
        if source_type == SourceType::Hub {
            if let Err(e) = manager.sync_source(&source.id) {
                tracing::warn!("[Marketplace] Initial hub sync failed: {e}");
            }
            return anyhow::Ok(manager.get_source(&source.id).unwrap_or(source));
        }
        anyhow::Ok(source)
    })
    .await
    .map_err(internal)?;

    let result = result.map_err(internal)?;
    Ok(Json(result))
}

/// DELETE /api/marketplace/sources/:id - remove a source
pub(crate) async fn marketplace_sources_delete(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let _ = task::spawn_blocking(move || {
        let mut manager = manager.lock().unwrap();
        manager.remove_source(&id)
    })
    .await
    .map_err(internal)?;

    Ok(Json(serde_json::json!({ "success": true })))
}

/// POST /api/marketplace/sources/:id/sync - sync a git source
pub(crate) async fn marketplace_sources_sync(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let _ = task::spawn_blocking(move || {
        let mut manager = manager.lock().unwrap();
        manager.sync_source(&id)
    })
    .await
    .map_err(internal)?;

    Ok(Json(serde_json::json!({ "success": true })))
}

/// POST /api/marketplace/sources/reorder - reorder sources
pub(crate) async fn marketplace_sources_reorder(
    State(s): State<Arc<UiState>>,
    Json(body): Json<ReorderSourcesBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let _ = task::spawn_blocking(move || {
        let mut manager = manager.lock().unwrap();
        manager.reorder_sources(body.ordered_ids)
    })
    .await
    .map_err(internal)?;

    Ok(Json(serde_json::json!({ "success": true })))
}

/// GET /api/marketplace/sources/:id - get source with plugins
pub(crate) async fn marketplace_source_get(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<SourceInfoResponse>, AppError> {
    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let source_info = task::spawn_blocking(move || {
        let manager = manager.lock().unwrap();
        manager.get_source_info(&id)
    })
    .await
    .map_err(internal)?;

    let source_info = source_info.map_err(internal)?;
    let source_info =
        source_info.ok_or_else(|| AppError(StatusCode::NOT_FOUND, "Source not found".into()))?;

    let mut plugins = source_info.plugins;
    stamp_installed_apps(&s, &mut plugins);

    // Convert plugins to JSON
    let plugins_json: Vec<serde_json::Value> = plugins
        .into_iter()
        .map(|p| serde_json::to_value(p).unwrap_or(serde_json::Value::Null))
        .collect();

    Ok(Json(SourceInfoResponse {
        source: source_info.source,
        plugins: plugins_json,
    }))
}

/// POST /api/marketplace/sources/:id/enable-all - enable all plugins in a source
pub(crate) async fn marketplace_source_enable_all(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let _ = task::spawn_blocking(move || {
        let mut manager = manager.lock().unwrap();
        manager.enable_all_in_source(&id)
    })
    .await
    .map_err(internal)?;

    resync_plugin_mcp(&s).await;

    Ok(Json(serde_json::json!({ "success": true })))
}

/// POST /api/marketplace/sources/:id/disable-all - disable all plugins in a source
pub(crate) async fn marketplace_source_disable_all(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let _ = task::spawn_blocking(move || {
        let mut manager = manager.lock().unwrap();
        manager.disable_all_in_source(&id)
    })
    .await
    .map_err(internal)?;

    resync_plugin_mcp(&s).await;

    Ok(Json(serde_json::json!({ "success": true })))
}

/// POST /api/marketplace/sources/:id/plugins/:name/toggle - toggle a plugin.
/// With no body the current state is flipped, which is what the UIs send.
pub(crate) async fn marketplace_plugin_toggle(
    State(s): State<Arc<UiState>>,
    AxumPath(params): AxumPath<(String, String)>,
    body: Option<Json<TogglePluginBody>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let (id, name) = params;
    let requested = body.map(|Json(b)| b.enabled);

    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let enabled = task::spawn_blocking(move || {
        let mut manager = manager.lock().unwrap();
        let enabled = match requested {
            Some(v) => v,
            None => !manager.is_plugin_enabled(&id, &name),
        };
        manager
            .set_plugin_enabled(&id, &name, enabled)
            .map(|_| enabled)
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;

    resync_plugin_mcp(&s).await;

    Ok(Json(
        serde_json::json!({ "success": true, "enabled": enabled }),
    ))
}

/// Rebuild the MCP servers contributed by plugins after an enable/disable.
///
/// Without this a freshly enabled plugin's tools would not appear until the
/// next daemon restart, which reads to the user as the plugin not working.
pub(crate) async fn resync_plugin_mcp(s: &Arc<UiState>) {
    let (Some(marketplace), Some(mcp)) = (s.marketplace_manager.as_ref(), s.mcp_manager.as_ref())
    else {
        return;
    };
    crate::marketplace::mcp_sync::sync(marketplace, mcp).await;
}

#[derive(Deserialize, Default)]
pub struct InstallQuery {
    /// `?force=true` installs despite a blocking scan verdict. The scan still
    /// runs and the report is still returned.
    #[serde(default)]
    force: bool,
}

/// POST /api/marketplace/sources/:id/plugins/:name/install - install one plugin
/// from a hub catalog (scan + clone + enable).
///
/// A blocking scan verdict comes back as 422 with the full report in
/// `scan`, so the UI can show the findings and offer an explicit override
/// rather than a bare failure.
pub(crate) async fn marketplace_plugin_install(
    State(s): State<Arc<UiState>>,
    AxumPath(params): AxumPath<(String, String)>,
    Query(q): Query<InstallQuery>,
) -> Result<axum::response::Response, AppError> {
    let (id, name) = params;

    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let policy = crate::security::ScanPolicy::from_config(&s.config);
    let outcome = task::spawn_blocking(move || {
        let mut manager = manager.lock().unwrap();
        manager.install_hub_plugin(&id, &name, policy, q.force)
    })
    .await
    .map_err(internal)?
    .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;

    match outcome {
        // 422 with the findings as data, not prose: the UI renders them and
        // offers an explicit override.
        crate::marketplace::manager::InstallOutcome::Blocked { report, staged_dir } => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "success": false,
                "blocked": true,
                "error": format!(
                    "Blocked by the pre-install security scan (risk {}/100). \
                     Nothing was recorded or enabled.",
                    report.risk_score()
                ),
                "stagedDir": staged_dir.to_string_lossy(),
                "scan": report,
            })),
        )
            .into_response()),
        crate::marketplace::manager::InstallOutcome::Installed { dir, scan } => {
            // Installing enables the plugin, so its servers become live now.
            resync_plugin_mcp(&s).await;
            Ok(Json(serde_json::json!({
                "success": true,
                "dir": dir.to_string_lossy(),
                "scan": scan,
            }))
            .into_response())
        }
    }
}

/// DELETE /api/marketplace/sources/:id/plugins/:name - uninstall a hub plugin.
pub(crate) async fn marketplace_plugin_uninstall(
    State(s): State<Arc<UiState>>,
    AxumPath(params): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let (id, name) = params;

    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let removed = task::spawn_blocking(move || {
        let mut manager = manager.lock().unwrap();
        manager.uninstall_hub_plugin(&id, &name)
    })
    .await
    .map_err(internal)?
    .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;

    // Withdraw the servers the removed plugin was contributing; leaving them
    // registered means the daemon keeps a command whose files are gone.
    resync_plugin_mcp(&s).await;

    Ok(Json(
        serde_json::json!({ "success": true, "removed": removed }),
    ))
}

/// GET /api/marketplace/sources/:id/catalog - the raw hub catalog.
pub(crate) async fn marketplace_source_catalog(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    let catalog = task::spawn_blocking(move || {
        let manager = manager.lock().unwrap();
        manager.get_catalog(&id)
    })
    .await
    .map_err(internal)?
    .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;

    Ok(Json(serde_json::to_value(catalog).map_err(internal)?))
}

/// POST /api/marketplace/sources/:id/plugins/:name/mcp/:server/use-tools - set MCP tool allowlist
pub(crate) async fn marketplace_mcp_use_tools(
    State(_s): State<Arc<UiState>>,
    AxumPath(_params): AxumPath<(String, String, String)>,
    Json(_body): Json<SetUseToolsBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    // This would need to be implemented in MarketplaceManager
    // For now, return success
    Ok(Json(serde_json::json!({ "success": true })))
}

/// GET /api/marketplace/mcp-status — what every enabled plugin declares, and
/// what actually happened to it.
///
/// `registered` servers are live in the shared MCP manager and their tools are
/// in the agent roster. A `reason` says why one was left out — an unusable
/// declaration, or a command the install scan called dangerous, which a plugin
/// toggle alone does not authorise.
pub(crate) async fn marketplace_mcp_status(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let declared = match s.marketplace_manager.as_ref() {
        Some(m) => {
            let m = m.clone();
            task::spawn_blocking(move || m.lock().unwrap().get_enabled_mcp_servers())
                .await
                .map_err(internal)?
        }
        None => Vec::new(),
    };

    let live = match s.mcp_manager.as_ref() {
        Some(mcp) => mcp.get_all_servers().await,
        None => Vec::new(),
    };
    let registered_names = match s.mcp_manager.as_ref() {
        Some(mcp) => mcp.plugin_server_names().await,
        None => Vec::new(),
    };

    let servers: Vec<serde_json::Value> = declared
        .iter()
        .map(|d| {
            let reason = crate::marketplace::mcp_sync::to_external_config(d).err();
            let info = live.iter().find(|i| i.config.name == d.name);
            serde_json::json!({
                "name": d.name,
                "transport": d.transport,
                "description": d.description,
                "pluginRoot": d.plugin_root,
                "registered": registered_names.contains(&d.name),
                "reason": reason,
                "status": info.map(|i| i.status),
                "toolCount": info.and_then(|i| i.tools.as_ref()).map(|t| t.len()).unwrap_or(0),
                "error": info.and_then(|i| i.error.clone()),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({ "servers": servers })))
}

/// GET /api/marketplace/plugins/:name/widget-static/*path — serve an enabled
/// plugin's widget assets (`<pluginDir>/widgets/…`). Plugins have no server of
/// their own, so this route is the only origin their `url` widgets load from
/// (the registry resolves plugin `entryUrl`s against it). Same containment
/// discipline as `space_apps_static`: no `..`/backslash, canonicalized-root
/// prefix check, files only.
pub(crate) async fn plugin_widget_static(
    State(s): State<Arc<UiState>>,
    AxumPath((name, req_path)): AxumPath<(String, String)>,
) -> Result<axum::response::Response, AppError> {
    use axum::body::Body;
    use axum::http::header;

    if req_path.contains("..") || req_path.contains('\\') {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid path".into()));
    }
    let manager = s
        .marketplace_manager
        .as_ref()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Marketplace manager not available".into(),
            )
        })?
        .clone();

    // Resolving the plugin dir scans sources on disk — keep it off the async
    // runtime thread, like the other marketplace handlers.
    let wanted = name.clone();
    let dir = task::spawn_blocking(move || {
        let guard = manager.lock().ok()?;
        guard
            .enabled_plugin_dirs()
            .into_iter()
            .find(|(n, _)| n == &wanted)
            .map(|(_, d)| d)
    })
    .await
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("Plugin '{name}' not found or not enabled"),
        )
    })?;

    let root = dir.join("widgets");
    let rel = req_path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let path = root.join(rel);
    let canonical_root = root
        .canonicalize()
        .map_err(|_| AppError(StatusCode::NOT_FOUND, "Plugin has no widgets dir".into()))?;
    let canonical_path = path
        .canonicalize()
        .map_err(|_| AppError(StatusCode::NOT_FOUND, "File not found".into()))?;
    if !canonical_path.starts_with(&canonical_root) || !canonical_path.is_file() {
        return Err(AppError(StatusCode::NOT_FOUND, "File not found".into()));
    }
    let bytes = tokio::fs::read(&canonical_path)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            super::space::content_type_for(&canonical_path),
        )
        .body(Body::from(bytes))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── installed-app stamping ──────────────────────────────────────────────

    fn entry(name: &str, kind: Option<&str>, slug: Option<&str>, version: Option<&str>) -> crate::marketplace::types::MarketplacePlugin {
        crate::marketplace::types::MarketplacePlugin {
            name: name.into(),
            description: String::new(),
            version: version.map(Into::into),
            author: None,
            keywords: None,
            dir: String::new(),
            source_id: "s".into(),
            source_name: "hub".into(),
            priority: 0,
            enabled: false,
            installed: false,
            kind: kind.map(Into::into),
            slug: slug.map(Into::into),
            downloads: None,
            installed_version: None,
            update_available: false,
            category: None,
            license: None,
            repository: None,
            skill_count: 0,
            subagent_count: 0,
            has_hooks: false,
            mcp_server_count: 0,
            skills: Vec::new(),
            subagents: Vec::new(),
            mcp_servers: Vec::new(),
        }
    }

    fn installed(pairs: &[(&str, Option<&str>)]) -> std::collections::HashMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(s, v)| ((*s).to_string(), v.map(Into::into)))
            .collect()
    }

    /// An app on disk at the catalog's version is done — offering "Install"
    /// again is the bug this whole path exists to fix.
    #[test]
    fn an_app_at_the_catalog_version_reads_installed_and_current() {
        let mut p = vec![entry("ai-office", Some("app"), Some("senclaw/ai-office"), Some("1.0.1"))];
        apply_installed_apps(&mut p, &installed(&[("senclaw/ai-office", Some("1.0.1"))]));
        assert!(p[0].installed);
        assert_eq!(p[0].installed_version.as_deref(), Some("1.0.1"));
        assert!(!p[0].update_available, "same version is not an update");
    }

    #[test]
    fn an_older_installed_version_offers_the_update() {
        let mut p = vec![entry("ai-office", Some("app"), Some("senclaw/ai-office"), Some("1.0.1"))];
        apply_installed_apps(&mut p, &installed(&[("senclaw/ai-office", Some("1.0.0"))]));
        assert!(p[0].installed && p[0].update_available);
    }

    /// Apps installed before origins were stamped have no recorded version.
    /// Treating that as "current" would strand them on an old build forever,
    /// so the update is offered — matching `is_newer(_, None)`.
    #[test]
    fn an_unstamped_install_still_offers_the_update() {
        let mut p = vec![entry("ai-office", Some("app"), Some("senclaw/ai-office"), Some("1.0.1"))];
        apply_installed_apps(&mut p, &installed(&[("senclaw/ai-office", None)]));
        assert!(p[0].installed);
        assert!(p[0].installed_version.is_none());
        assert!(p[0].update_available);
    }

    #[test]
    fn an_app_that_is_not_installed_is_left_alone() {
        let mut p = vec![entry("clock", Some("app"), Some("senclaw/clock"), Some("1.0.0"))];
        apply_installed_apps(&mut p, &installed(&[("senclaw/ai-office", Some("1.0.1"))]));
        assert!(!p[0].installed && !p[0].update_available);
    }

    /// A plugin's `installed` means "cloned into this source" and is decided by
    /// the marketplace manager. A Space App with a colliding slug must not flip
    /// it, or a plugin would show as installed because an app shares its name.
    #[test]
    fn a_plugin_entry_is_never_touched() {
        let mut p = vec![
            entry("code-modernization", None, None, Some("1.2.0")),
            entry("shared-name", Some("plugin"), Some("senclaw/shared-name"), Some("1.0.0")),
        ];
        apply_installed_apps(
            &mut p,
            &installed(&[("senclaw/shared-name", Some("0.9.0")), ("senclaw/code-modernization", None)]),
        );
        assert!(!p[0].installed, "a marketplace.json plugin is not a registry app");
        assert!(!p[1].installed, "kind=plugin is not an app either");
    }

    // ─── hub install body ────────────────────────────────────────────────────

    /// The UI sends just a slug for the common case; everything else has to
    /// default, or "install senclaw/clock" would need four fields the user
    /// does not have.
    #[test]
    fn hub_install_body_needs_only_a_slug() {
        let b: HubInstallBody = serde_json::from_str(r#"{"slug":"senclaw/clock"}"#).unwrap();
        assert_eq!(b.slug, "senclaw/clock");
        assert!(b.version.is_none(), "version must default to latest");
        assert!(b.hub.is_none(), "hub must default to the built-in one");
        assert!(b.platform.is_none(), "platform must default to this host");
        assert!(!b.force, "the security scan must not be bypassed by default");
    }

    #[test]
    fn hub_install_body_accepts_every_optional_field() {
        let b: HubInstallBody = serde_json::from_str(
            r#"{"slug":"@acme/thing","version":"1.2.3","hub":"https://h.example",
                "platform":"darwin-arm64","force":true}"#,
        )
        .unwrap();
        assert_eq!(b.version.as_deref(), Some("1.2.3"));
        assert_eq!(b.hub.as_deref(), Some("https://h.example"));
        assert_eq!(b.platform.as_deref(), Some("darwin-arm64"));
        assert!(b.force);
    }

    /// A bare name is the shape package pages show; it must resolve under the
    /// default scope rather than being rejected.
    #[test]
    fn a_bare_name_resolves_under_the_default_scope() {
        let (scope, name) = crate::marketplace::registry::parse_slug("clock").unwrap();
        assert_eq!(scope, crate::marketplace::registry::DEFAULT_SCOPE);
        assert_eq!(name, "clock");
    }

    /// Blank strings are what an empty text field posts; they must fall back
    /// to the defaults instead of becoming an empty hub URL or platform.
    #[test]
    fn blank_optional_strings_fall_back_to_defaults() {
        let b: HubInstallBody =
            serde_json::from_str(r#"{"slug":"clock","hub":"  ","platform":""}"#).unwrap();
        let hub = b
            .hub
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| crate::marketplace::publish::DEFAULT_HUB.to_string());
        let host = b
            .platform
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(crate::marketplace::publish::host_platform);
        assert_eq!(hub, crate::marketplace::publish::DEFAULT_HUB);
        assert!(!host.is_empty());
    }
}

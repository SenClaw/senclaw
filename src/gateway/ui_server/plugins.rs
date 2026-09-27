//! REST handlers for plugin management.
//! Routes registered under /api/plugins/* in core.rs.

use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::Json,
};
use chrono::Utc;
use serde::Deserialize;

use super::core::{AppError, UiState};
use crate::clawhub::client::{download_skill_zip, get_skill_meta, search_skills, DEFAULT_REGISTRY};
use crate::clawhub::lockfile::extract_zip_to_dir;
use crate::plugins::db::{
    delete_plugin, get_plugin, get_runtime, list_plugins, set_plugin_enabled, update_plugin_config,
    upsert_plugin, upsert_runtime, InstalledPlugin, PluginRuntime,
};
use crate::plugins::manifest::parse_plugin_md;

fn db(s: &UiState) -> Result<&crate::db::Db, AppError> {
    s.db.as_deref()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))
}

fn internal(e: impl std::fmt::Display) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn registry(_s: &UiState) -> String {
    std::env::var("CLAWHUB_REGISTRY")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string())
}

// ─── Types ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct RemoteSearchQuery {
    q: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PluginInstallBody {
    slug: String,
    /// JSON object of env var name → value (keys must match manifest.env_vars)
    #[serde(default)]
    config_json: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PluginConfigureBody {
    config_json: String,
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// GET /api/plugins — list installed plugins with runtime status
pub(crate) async fn plugins_list(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let rows = list_plugins(db).map_err(internal)?;

    // Merge runtime status
    let result: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|p| {
            let rt = get_runtime(db, &p.slug).unwrap_or(None);
            serde_json::json!({
                "slug": p.slug,
                "display_name": p.display_name,
                "summary": p.summary,
                "version": p.version,
                "plugin_type": p.plugin_type,
                "registry": p.registry,
                "enabled": p.enabled,
                "installed_at": p.installed_at,
                "config_json": p.config_json,
                "status": rt.as_ref().map(|r| r.status.as_str()).unwrap_or("stopped"),
                "pid": rt.as_ref().and_then(|r| r.pid),
                "error_msg": rt.as_ref().and_then(|r| r.error_msg.as_deref()),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({ "plugins": result })))
}

/// GET /api/plugins/remote-search?q=
pub(crate) async fn plugins_remote_search(
    State(s): State<Arc<UiState>>,
    Query(q): Query<RemoteSearchQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let query = q.q.unwrap_or_default();
    if query.trim().is_empty() {
        return Ok(Json(serde_json::json!({ "results": [] })));
    }

    let reg = registry(&s);
    let raw = search_skills(&query, Some(&reg), Some(20), None)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?;

    let db = db(&s)?;
    let installed = list_plugins(db).unwrap_or_default();
    let installed_slugs: std::collections::HashSet<&str> =
        installed.iter().map(|p| p.slug.as_str()).collect();

    let results: Vec<serde_json::Value> = raw
        .into_iter()
        .map(|r| {
            let mut v = serde_json::to_value(&r).unwrap_or_default();
            v["installed"] = serde_json::Value::Bool(installed_slugs.contains(r.slug.as_str()));
            v
        })
        .collect();

    Ok(Json(serde_json::json!({ "results": results })))
}

/// POST /api/plugins/install
pub(crate) async fn plugins_install(
    State(s): State<Arc<UiState>>,
    Json(body): Json<PluginInstallBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let slug = body.slug.trim().to_string();
    if slug.is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "slug required".into()));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(AppError(StatusCode::BAD_REQUEST, "invalid slug".into()));
    }

    let managed_dir = &s.config.paths.managed_plugins_dir;
    let target = managed_dir.join(&slug);
    let canonical_managed = managed_dir
        .canonicalize()
        .unwrap_or_else(|_| managed_dir.clone());
    if target.exists() {
        if !target
            .canonicalize()
            .unwrap_or_else(|_| target.clone())
            .starts_with(&canonical_managed)
        {
            return Err(AppError(StatusCode::BAD_REQUEST, "invalid slug".into()));
        }
    }

    let reg = registry(&s);
    let meta = get_skill_meta(&slug, Some(&reg), None)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?;

    if meta
        .moderation
        .as_ref()
        .map_or(false, |m| m.is_malware_blocked)
    {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            format!("{slug} is flagged as malicious"),
        ));
    }

    let version = meta
        .latest_version
        .as_ref()
        .map(|v| v.version.clone())
        .ok_or_else(|| {
            AppError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "no version available".into(),
            )
        })?;

    let zip_buf = download_skill_zip(&slug, &version, Some(&reg), None)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?;

    if target.exists() {
        let _ = tokio::fs::remove_dir_all(&target).await;
    }
    extract_zip_to_dir(&zip_buf, &target)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Parse manifest
    let manifest = parse_plugin_md(&target.join("PLUGIN.md"));
    let plugin_type = manifest
        .as_ref()
        .map(|m| m.plugin_type.as_str().to_string())
        .unwrap_or_else(|| "mcp_server".to_string());
    let manifest_json = manifest
        .as_ref()
        .and_then(|m| serde_json::to_string(m).ok());

    let config_json = body.config_json.unwrap_or_else(|| "{}".to_string());
    let now = now_ms();

    let db = db(&s)?;
    upsert_plugin(
        db,
        &InstalledPlugin {
            slug: slug.clone(),
            display_name: manifest.as_ref().and_then(|m| m.display_name.clone()),
            summary: manifest.as_ref().and_then(|m| m.description.clone()),
            version: version.clone(),
            plugin_type: plugin_type.clone(),
            registry: reg,
            enabled: true,
            installed_at: now,
            updated_at: now,
            config_json: config_json.clone(),
            manifest_json,
        },
    )
    .map_err(internal)?;

    upsert_runtime(
        db,
        &PluginRuntime {
            slug: slug.clone(),
            status: "stopped".to_string(),
            pid: None,
            port: None,
            started_at: None,
            error_msg: None,
            last_ping: None,
        },
    )
    .map_err(internal)?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "slug": slug,
        "version": version,
        "plugin_type": plugin_type,
    })))
}

/// GET /api/plugins/:slug
pub(crate) async fn plugins_get(
    State(s): State<Arc<UiState>>,
    AxumPath(slug): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let plugin = get_plugin(db, &slug)
        .map_err(internal)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "plugin not found".into()))?;
    let rt = get_runtime(db, &slug).unwrap_or(None);

    Ok(Json(serde_json::json!({
        "slug": plugin.slug,
        "display_name": plugin.display_name,
        "summary": plugin.summary,
        "version": plugin.version,
        "plugin_type": plugin.plugin_type,
        "registry": plugin.registry,
        "enabled": plugin.enabled,
        "installed_at": plugin.installed_at,
        "config_json": plugin.config_json,
        "status": rt.as_ref().map(|r| r.status.as_str()).unwrap_or("stopped"),
        "pid": rt.as_ref().and_then(|r| r.pid),
        "error_msg": rt.as_ref().and_then(|r| r.error_msg.as_deref()),
        "started_at": rt.as_ref().and_then(|r| r.started_at),
    })))
}

/// DELETE /api/plugins/:slug — uninstall
pub(crate) async fn plugins_uninstall(
    State(s): State<Arc<UiState>>,
    AxumPath(slug): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    get_plugin(db, &slug)
        .map_err(internal)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "plugin not found".into()))?;

    let target = s.config.paths.managed_plugins_dir.join(&slug);
    if target.exists() {
        tokio::fs::remove_dir_all(&target).await.map_err(internal)?;
    }

    delete_plugin(db, &slug).map_err(internal)?;
    Ok(Json(serde_json::json!({ "ok": true, "slug": slug })))
}

/// POST /api/plugins/:slug/enable
pub(crate) async fn plugins_enable(
    State(s): State<Arc<UiState>>,
    AxumPath(slug): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    get_plugin(db, &slug)
        .map_err(internal)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "plugin not found".into()))?;
    set_plugin_enabled(db, &slug, true).map_err(internal)?;
    Ok(Json(serde_json::json!({ "slug": slug, "enabled": true })))
}

/// POST /api/plugins/:slug/disable
pub(crate) async fn plugins_disable(
    State(s): State<Arc<UiState>>,
    AxumPath(slug): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    get_plugin(db, &slug)
        .map_err(internal)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "plugin not found".into()))?;
    set_plugin_enabled(db, &slug, false).map_err(internal)?;
    Ok(Json(serde_json::json!({ "slug": slug, "enabled": false })))
}

/// POST /api/plugins/:slug/configure
pub(crate) async fn plugins_configure(
    State(s): State<Arc<UiState>>,
    AxumPath(slug): AxumPath<String>,
    Json(body): Json<PluginConfigureBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    get_plugin(db, &slug)
        .map_err(internal)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "plugin not found".into()))?;

    // Validate it's valid JSON object
    let val: serde_json::Value = serde_json::from_str(&body.config_json).map_err(|_| {
        AppError(
            StatusCode::BAD_REQUEST,
            "config_json must be a JSON object".into(),
        )
    })?;
    if !val.is_object() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "config_json must be a JSON object".into(),
        ));
    }

    update_plugin_config(db, &slug, &body.config_json).map_err(internal)?;
    Ok(Json(serde_json::json!({ "ok": true, "slug": slug })))
}

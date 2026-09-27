//! `/api/widgets` + `/api/defaults` — widget catalog, per-widget enable
//! toggle, and the default-flow settings. Backs Plugins → Widget in the web
//! UI.
//!
//! The catalog is recomputed per request from the same sources the
//! process-wide registry uses (built-ins + `space_apps` manifests + enabled
//! plugins); handlers build a scoped [`WidgetRegistry`] from `UiState` parts
//! instead of relying on the global so bare test setups work too.

use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;

use crate::gateway::group_manager::{
    get_defaults_config, save_defaults_config, set_widget_disabled, DefaultsConfig,
};
use crate::widgets::WidgetRegistry;

use super::core::{AppError, UiState};

fn registry_for(s: &UiState) -> WidgetRegistry {
    WidgetRegistry::new(
        s.db.clone(),
        s.config.paths.global_config_path.clone(),
        s.marketplace_manager.clone(),
    )
}

/// GET /api/widgets — the full catalog with enabled flags.
pub(crate) async fn widgets_list(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let reg = registry_for(&s);
    let catalog = tokio::task::spawn_blocking(move || reg.catalog())
        .await
        .unwrap_or_default();
    Json(serde_json::json!({ "widgets": catalog }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct WidgetToggleBody {
    pub enabled: bool,
}

/// PUT /api/widgets/:id — enable/disable one widget (full id, e.g.
/// `crm.pipeline` or the builtin `chart`).
pub(crate) async fn widget_toggle(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<WidgetToggleBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let path = &s.config.paths.global_config_path;
    set_widget_disabled(path, &id, !body.enabled)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "id": id, "enabled": body.enabled })))
}

fn defaults_json(cfg: &DefaultsConfig) -> serde_json::Value {
    serde_json::json!({
        "openLink": cfg.effective_open_link(),
        "media": cfg.effective_media(),
        "search": cfg.effective_search(),
        "searchEngine": cfg.effective_search_engine(),
        "note": cfg.effective_note(),
        "disabledWidgets": cfg.disabled_widgets.clone().unwrap_or_default(),
    })
}

/// GET /api/defaults — effective default-flow settings (fallbacks applied).
pub(crate) async fn defaults_get(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let cfg = get_defaults_config(&s.config.paths.global_config_path);
    Json(defaults_json(&cfg))
}

/// PUT /api/defaults — partial update; only fields present in the body change.
pub(crate) async fn defaults_set(
    State(s): State<Arc<UiState>>,
    Json(patch): Json<DefaultsConfig>,
) -> Result<Json<serde_json::Value>, AppError> {
    let path = &s.config.paths.global_config_path;
    let merged = save_defaults_config(path, &patch)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(defaults_json(&merged)))
}

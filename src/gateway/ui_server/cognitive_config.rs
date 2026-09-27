//! Settings → Cognitive endpoints. Reads/writes
//! [`crate::gateway::group_manager::PersistedCognitiveConfig`] in
//! `global_config.json`. Apply requires a daemon restart (the
//! `CognitiveSystem` semaphore is sized at boot from `Config.cognitive`),
//! same as the embedding-provider settings.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::Json};
use serde::Deserialize;

use crate::gateway::group_manager::{
    load_cognitive_config, save_cognitive_config, PersistedCognitiveConfig,
};

use super::core::{AppError, UiState};

#[derive(Debug, Default, Deserialize)]
pub(crate) struct CognitiveConfigBody {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default, rename = "maxConcurrent")]
    pub max_concurrent: Option<usize>,
    #[serde(default, rename = "maxOutputChars")]
    pub max_output_chars: Option<usize>,
    #[serde(default, rename = "reflectMinChars")]
    pub reflect_min_chars: Option<usize>,
    #[serde(default, rename = "reflectMaxChars")]
    pub reflect_max_chars: Option<usize>,
    #[serde(default, rename = "reflectCooldownMs")]
    pub reflect_cooldown_ms: Option<u64>,
    #[serde(default, rename = "reflectWindowIdleMs")]
    pub reflect_window_idle_ms: Option<u64>,
    #[serde(default, rename = "autoReflection")]
    pub auto_reflection: Option<bool>,
    #[serde(default, rename = "maintenanceIntervalHours")]
    pub maintenance_interval_hours: Option<u64>,
}

/// GET /api/cognitive-config — returns the persisted UI form values plus
/// the *effective* values currently in use by the live daemon, so the
/// UI can show "saved=400 / effective=400 (restart required to apply)".
pub(crate) async fn cognitive_config_get(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let stored = load_cognitive_config(&s.config.paths.global_config_path).unwrap_or_default();
    Json(serde_json::json!({
        "saved": {
            "enabled": stored.enabled,
            "maxConcurrent": stored.max_concurrent,
            "maxOutputChars": stored.max_output_chars,
            "reflectMinChars": stored.reflect_min_chars,
            "reflectMaxChars": stored.reflect_max_chars,
            "reflectCooldownMs": stored.reflect_cooldown_ms,
            "reflectWindowIdleMs": stored.reflect_window_idle_ms,
            "autoReflection": stored.auto_reflection,
            "maintenanceIntervalHours": stored.maintenance_interval_hours,
        },
        // Effective view from the live daemon — what cognify actually uses
        // right now. Useful for the UI to flag "restart needed" when this
        // diverges from `saved`.
        "effective": {
            "enabled": s.config.cognitive.enabled,
            "maxConcurrent": s.config.cognitive.max_concurrent,
            "maxOutputChars": s.config.cognitive.max_output_chars,
            "reflectMinChars": s.config.cognitive.reflect_min_chars,
            "reflectMaxChars": s.config.cognitive.reflect_max_chars,
            "reflectCooldownMs": s.config.cognitive.reflect_cooldown_ms,
            "reflectWindowIdleMs": s.config.cognitive.reflect_window_idle_ms,
            "autoReflection": s.config.memory.cognitive_reflection,
            "maintenanceIntervalHours": s.config.cognitive.maintenance_interval_hours,
        },
    }))
}

/// POST /api/cognitive-config — persist UI form values. Returns the
/// saved payload echoed back.
pub(crate) async fn cognitive_config_save(
    State(s): State<Arc<UiState>>,
    Json(body): Json<CognitiveConfigBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let persisted = PersistedCognitiveConfig {
        enabled: body.enabled,
        max_concurrent: body.max_concurrent,
        max_output_chars: body.max_output_chars,
        reflect_min_chars: body.reflect_min_chars,
        reflect_max_chars: body.reflect_max_chars,
        reflect_cooldown_ms: body.reflect_cooldown_ms,
        reflect_window_idle_ms: body.reflect_window_idle_ms,
        auto_reflection: body.auto_reflection,
        maintenance_interval_hours: body.maintenance_interval_hours,
    };
    save_cognitive_config(&s.config.paths.global_config_path, &persisted)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::to_value(&persisted).unwrap_or_default()))
}

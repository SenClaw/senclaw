use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::Json};
use serde::Deserialize;

use crate::gateway::group_manager::{
    load_embedding_config, save_embedding_config, EmbeddingConfig,
};

use super::core::{AppError, UiState};

#[derive(Deserialize)]
pub(crate) struct EmbeddingConfigBody {
    pub provider: String,
    #[serde(rename = "apiKey", default)]
    pub api_key: String,
    #[serde(rename = "baseURL", default)]
    pub base_url: String,
    #[serde(rename = "modelName", default)]
    pub model_name: String,
    #[serde(rename = "modelPath", default)]
    pub model_path: String,
    pub dimensions: Option<u32>,
}

/// GET /api/embedding-config
pub(crate) async fn embedding_config_get(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    match load_embedding_config(&s.config.paths.global_config_path) {
        Some(cfg) => Json(serde_json::to_value(&cfg).unwrap_or_default()),
        None => Json(serde_json::json!({
            "provider": "none",
            "apiKey": "",
            "baseURL": "",
            "modelName": "",
            "modelPath": "",
            "dimensions": null
        })),
    }
}

/// POST /api/embedding-config
pub(crate) async fn embedding_config_save(
    State(s): State<Arc<UiState>>,
    Json(body): Json<EmbeddingConfigBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let cfg = EmbeddingConfig {
        provider: body.provider,
        api_key: body.api_key,
        base_url: body.base_url,
        model_name: body.model_name,
        model_path: body.model_path,
        dimensions: body.dimensions,
    };
    save_embedding_config(&s.config.paths.global_config_path, &cfg)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::to_value(&cfg).unwrap_or_default()))
}

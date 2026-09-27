//! `/api/lsp/*` — what the language-server integration is doing, and its
//! settings file (`~/.senclaw/lsp.json`).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;

use super::core::{AppError, UiState};

/// GET /api/lsp/status
pub(crate) async fn lsp_status(State(_s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    Json(crate::lsp::status().await)
}

/// GET /api/lsp/settings
pub(crate) async fn lsp_settings_get(State(_s): State<Arc<UiState>>) -> Json<crate::lsp::LspSettings> {
    Json(crate::lsp::load_settings())
}

/// PUT /api/lsp/settings — whole document; unknown languages are kept as
/// given so a user can add a server the built-in table lacks.
pub(crate) async fn lsp_settings_put(
    State(_s): State<Arc<UiState>>,
    Json(body): Json<crate::lsp::LspSettings>,
) -> Result<Json<serde_json::Value>, AppError> {
    if body.timeout_ms < 500 || body.timeout_ms > 60_000 {
        return Err(AppError(StatusCode::BAD_REQUEST, "timeoutMs must be between 500 and 60000".into()));
    }
    crate::lsp::save_settings(&body)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

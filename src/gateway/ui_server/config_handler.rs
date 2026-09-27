use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::Json};
use serde::Deserialize;

use crate::gateway::group_manager::{
    get_admin_permissions_config, get_thinking_enabled, save_admin_permissions_config,
    save_thinking_enabled, AdminPermissions,
};

use super::core::{AppError, UiState};
use super::types::AdminPermissionsConfig;

// ===== /api/config =====

pub(crate) async fn config_handler(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let admin_perms = get_admin_permissions_config(&s.config.paths.global_config_path);
    Json(serde_json::json!({
        // Daemon release version — same identity as the git tag / Cargo version.
        // The desktop app compares this against its own build to detect a
        // daemon left over from an older bundle.
        "version": env!("CARGO_PKG_VERSION"),
        "wsPort": s.ws_port,
        "token": s.ws_token,
        // True when the daemon is bound beyond loopback and gates /api/* +
        // the WS ports behind the API token. This endpoint itself is only
        // reachable pre-auth from loopback, so the flag (and ws token above)
        // never leak to unauthenticated remote peers.
        "authRequired": s.api_auth.required(),
        // Where the desktop tray must write screen captures. Sent rather than
        // assumed client-side: `SENCLAW_SCREENSHOTS_DIR` can move it, and a
        // tray writing elsewhere would 404 on every shot it serves back.
        "screenshotsDir": s.config.paths.screenshots_dir.to_string_lossy(),
        "thinkingEnabled": get_thinking_enabled(&s.config.paths.global_config_path),
        "skipMainAgentPermissions": admin_perms.skip_main_agent_permissions,
        "skipAllAgentsPermissions": admin_perms.skip_all_agents_permissions,
    }))
}

// ===== /api/thinking =====

#[derive(Deserialize)]
pub(crate) struct ThinkingBody {
    enabled: bool,
}

pub(crate) async fn thinking_handler(
    State(s): State<Arc<UiState>>,
    Json(body): Json<ThinkingBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    save_thinking_enabled(&s.config.paths.global_config_path, body.enabled)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(ref api) = s.agent_api {
        api.set_thinking_enabled(body.enabled);
    }
    Ok(Json(serde_json::json!({ "thinkingEnabled": body.enabled })))
}

// ===== /api/admin-permissions =====

pub(crate) async fn admin_perms_get(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let cfg = get_admin_permissions_config(&s.config.paths.global_config_path);
    Json(serde_json::json!({
        "skipMainAgentPermissions": cfg.skip_main_agent_permissions,
        "skipAllAgentsPermissions": cfg.skip_all_agents_permissions,
    }))
}

pub(crate) async fn admin_perms_set(
    State(s): State<Arc<UiState>>,
    Json(body): Json<AdminPermissionsConfig>,
) -> Result<Json<serde_json::Value>, AppError> {
    let perm = AdminPermissions {
        skip_main_agent_permissions: body.skip_main_agent_permissions,
        skip_all_agents_permissions: body.skip_all_agents_permissions,
    };
    save_admin_permissions_config(&s.config.paths.global_config_path, &perm)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(ref api) = s.agent_api {
        api.set_permissions_config(body.clone());
    }
    Ok(Json(serde_json::to_value(body).unwrap_or_default()))
}

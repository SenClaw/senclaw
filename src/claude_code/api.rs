//! `/api/claude-code/*` — status (binary found, version, how to connect
//! Claude Code to SenClaw's MCP), live/recent runs, and settings.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use super::settings::{self, ClaudeCodeSettings};

pub struct ApiState {
    pub config: Arc<crate::config::Config>,
    /// Daemon base URL as Claude Code on this machine reaches it.
    pub base_url: String,
}

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/api/claude-code/status", get(status))
        .route("/api/claude-code/runs", get(runs))
        .route("/api/claude-code/settings", get(settings_get).put(settings_put))
        .with_state(state)
}

/// The `claude mcp add` commands for this daemon. The token is referenced as
/// an env var, never printed.
pub fn mcp_add_commands(base_url: &str) -> Vec<String> {
    ["core", "kanban"]
        .iter()
        .map(|s| {
            format!(
                "claude mcp add --transport http --scope user senclaw-{s} {base_url}/mcp/{s} --header \"Authorization: Bearer ${{SENCLAW_TOKEN}}\""
            )
        })
        .collect()
}

async fn status(State(st): State<Arc<ApiState>>) -> Response {
    let s = settings::load(&st.config.paths.global_config_path);
    let version = super::binary_version(&s.binary).await;
    Json(json!({
        "binary": s.binary,
        "installed": version.is_some(),
        "version": version,
        "assignee": "claude-code",
        "mcp": {
            "endpoints": [format!("{}/mcp/core", st.base_url), format!("{}/mcp/kanban", st.base_url)],
            "tokenFile": crate::util::paths::senclaw_home().join("api_token"),
            "addCommands": mcp_add_commands(&st.base_url),
        },
    }))
    .into_response()
}

async fn runs() -> Response {
    Json(json!({ "runs": super::registry::list() })).into_response()
}

async fn settings_get(State(st): State<Arc<ApiState>>) -> Response {
    Json(settings::load(&st.config.paths.global_config_path)).into_response()
}

async fn settings_put(State(st): State<Arc<ApiState>>, Json(body): Json<ClaudeCodeSettings>) -> Response {
    let s = match body.validated() {
        Ok(s) => s,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    };
    match crate::gateway::group_manager::save_claude_code_settings(&st.config.paths.global_config_path, &s) {
        Ok(()) => Json(s).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_commands_reference_the_token_by_name_only() {
        let c = mcp_add_commands("http://127.0.0.1:18788");
        assert_eq!(c.len(), 2);
        assert!(c[0].contains("http://127.0.0.1:18788/mcp/core"));
        assert!(c[0].contains("${SENCLAW_TOKEN}"));
    }
}

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::StatusCode,
    response::Json,
};
use serde::Deserialize;

use crate::mcp::config::{ExternalMcpServerConfig, McpScopeType, McpServerInfo, McpTransportType};
use crate::mcp::manager::McpManager;

use super::core::{AppError, UiState};

// ===== MCP helper =====

/// Helper to get McpManager from state.
pub(crate) fn mcp_mgr(s: &UiState) -> Result<&McpManager, AppError> {
    s.mcp_manager.as_ref().map(|m| m.as_ref()).ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "MCP manager not initialized".into(),
        )
    })
}

// ---- Helpers ----

/// Reject attempts to modify built-in servers.
fn reject_builtin(name: &str, mgr: &McpManager) -> Result<(), AppError> {
    if mgr.get_builtin_servers().iter().any(|b| b.name == name) {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            format!("{name} is a built-in server"),
        ));
    }
    Ok(())
}

fn mcp_server_json(info: &McpServerInfo) -> serde_json::Value {
    let mut v = serde_json::to_value(info).unwrap_or_default();
    v["builtin"] = serde_json::Value::Bool(false);
    v
}

// ---- GET /api/mcp-servers ----

pub(crate) async fn mcp_servers_list(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mgr = mcp_mgr(&s)?;
    let builtins = mgr.get_builtin_servers();
    let externals = mgr.get_all_servers().await;
    let mut servers: Vec<serde_json::Value> = builtins
        .iter()
        .map(|b| {
            serde_json::json!({
                "name": b.name,
                "transport": b.transport,
                "description": b.description,
                "builtin": true,
                "tools": b.tools,
            })
        })
        .collect();
    servers.extend(externals.iter().map(mcp_server_json));
    Ok(Json(serde_json::json!({ "servers": servers })))
}

// ---- GET /api/mcp-servers/:name ----

pub(crate) async fn mcp_servers_get(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mgr = mcp_mgr(&s)?;
    // Check builtins first
    if let Some(b) = mgr.get_builtin_servers().iter().find(|b| b.name == name) {
        return Ok(Json(serde_json::json!({
            "name": b.name,
            "transport": b.transport,
            "description": b.description,
            "builtin": true,
            "tools": b.tools,
        })));
    }
    let info = mgr.get_server_info(&name).await;
    if info.error.as_deref() == Some("server not found") {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("server {name} not found"),
        ));
    }
    Ok(Json(mcp_server_json(&info)))
}

// ---- POST /api/mcp-servers ----

#[derive(Deserialize)]
pub(crate) struct McpServerBody {
    name: String,
    transport: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(rename = "useTools")]
    use_tools: Option<Vec<String>>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    scope: Option<String>,
}

fn default_true() -> bool {
    true
}

pub(crate) async fn mcp_servers_save(
    State(s): State<Arc<UiState>>,
    Json(body): Json<McpServerBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mgr = mcp_mgr(&s)?;
    let transport = match body.transport.as_str() {
        "stdio" => McpTransportType::Stdio,
        "sse" => McpTransportType::Sse,
        "http" => McpTransportType::Http,
        _ => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "invalid transport type".into(),
            ))
        }
    };
    let scope = match body.scope.as_deref() {
        Some("project") => McpScopeType::Project,
        _ => McpScopeType::User,
    };
    let cfg = ExternalMcpServerConfig {
        name: body.name,
        transport,
        description: body.description,
        enabled: body.enabled,
        use_tools: body.use_tools,
        command: body.command,
        args: body.args,
        env: body.env,
        url: body.url,
        headers: body.headers,
    };

    // Validate before calling manager to map validation errors → 400
    if let Err(e) = cfg.validate() {
        return Err(AppError(StatusCode::BAD_REQUEST, e));
    }

    reject_builtin(&cfg.name, mgr)?;

    let info = mgr
        .add_or_update(cfg, scope)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(mcp_server_json(&info)))
}

// ---- DELETE /api/mcp-servers/:name ----

#[derive(Deserialize)]
pub(crate) struct McpDeleteQuery {
    scope: Option<String>,
}

pub(crate) async fn mcp_servers_delete(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
    Query(q): Query<McpDeleteQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mgr = mcp_mgr(&s)?;
    reject_builtin(&name, mgr)?;
    let scope = match q.scope.as_deref() {
        Some("project") => McpScopeType::Project,
        _ => McpScopeType::User,
    };
    let existed = mgr
        .remove(&name, scope)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !existed {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("server {name} not found"),
        ));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---- POST /api/mcp-servers/:name/connect ----

pub(crate) async fn mcp_servers_connect(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mgr = mcp_mgr(&s)?;
    reject_builtin(&name, mgr)?;
    match mgr.connect_server(&name).await {
        Ok(info) => Ok(Json(mcp_server_json(&info))),
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            Err(AppError(status, msg))
        }
    }
}

// ---- POST /api/mcp-servers/:name/disconnect ----

pub(crate) async fn mcp_servers_disconnect(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mgr = mcp_mgr(&s)?;
    reject_builtin(&name, mgr)?;
    mgr.disconnect_server(&name)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---- POST /api/mcp-servers/:name/tools ----

#[derive(Deserialize)]
pub(crate) struct McpToolsBody {
    #[serde(rename = "toolNames")]
    tool_names: Option<Vec<String>>,
    scope: Option<String>,
}

pub(crate) async fn mcp_servers_tools(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<McpToolsBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mgr = mcp_mgr(&s)?;
    reject_builtin(&name, mgr)?;
    let scope = match body.scope.as_deref() {
        Some("project") => McpScopeType::Project,
        _ => McpScopeType::User,
    };
    let found = mgr
        .update_use_tools(&name, scope, body.tool_names)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !found {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("server {name} not found"),
        ));
    }
    let info = mgr.get_server_info(&name).await;
    Ok(Json(mcp_server_json(&info)))
}

// ---- POST /api/mcp-servers/:name/enabled ----

#[derive(Deserialize)]
pub(crate) struct McpEnabledBody {
    enabled: bool,
    scope: Option<String>,
}

pub(crate) async fn mcp_servers_enabled(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<McpEnabledBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mgr = mcp_mgr(&s)?;
    reject_builtin(&name, mgr)?;
    let scope = match body.scope.as_deref() {
        Some("project") => McpScopeType::Project,
        _ => McpScopeType::User,
    };
    let found = mgr
        .update_enabled(&name, scope, body.enabled)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !found {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("server {name} not found"),
        ));
    }
    let info = mgr.get_server_info(&name).await;
    Ok(Json(mcp_server_json(&info)))
}

// ===== /api/hooks =====

pub(crate) async fn hooks_get(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let path = &s.config.paths.hooks_path;
    if path.exists() {
        let raw = fs::read_to_string(path)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let json: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({ "hooks": {} }));
        Ok(Json(json))
    } else {
        Ok(Json(serde_json::json!({ "hooks": {} })))
    }
}

pub(crate) async fn hooks_put(
    State(s): State<Arc<UiState>>,
    body: String,
) -> Result<Json<serde_json::Value>, AppError> {
    // Validate JSON and check for "hooks" key
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")))?;
    match &json {
        serde_json::Value::Object(map) if map.contains_key("hooks") => {}
        _ => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "Root object must have a \"hooks\" key".into(),
            ))
        }
    }
    // Validate hooks is an object
    if let Some(hooks) = json.get("hooks") {
        if !hooks.is_object() {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "\"hooks\" must be a plain object".into(),
            ));
        }
    }
    let path = &s.config.paths.hooks_path;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    fs::write(path, &body)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Engines read hooks once, at construction, and are cached per chat. Saving
    // the file is not enough: without this the edit reaches only conversations
    // started afterwards.
    if let Some(api) = s.agent_api.as_ref() {
        api.reload_all_hooks();
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---- POST /api/mcp-servers/:name/test ----

#[derive(Deserialize)]
pub(crate) struct TestToolBody {
    tool: String,
    #[serde(default)]
    args: serde_json::Value,
}

pub(crate) async fn mcp_servers_test(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
    axum::extract::Json(body): axum::extract::Json<TestToolBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mgr = mcp_mgr(&s)?;
    let args = if body.args.is_null() {
        serde_json::json!({})
    } else {
        body.args
    };
    match mgr.test_tool(&name, &body.tool, args).await {
        Ok(result) => Ok(Json(serde_json::json!({
            "ok": true,
            "result": result,
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
        }))),
    }
}

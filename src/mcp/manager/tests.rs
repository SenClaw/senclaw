//! Tests for the MCP manager module.

use super::*;
use crate::mcp::{ExternalMcpServerConfig, McpScopeType, McpServerStatus, McpTransportType};
use std::collections::HashMap;

#[test]
fn parse_valid_mcp_tool_name() {
    let result = parse_mcp_tool_name("mcp__my-server__my-tool");
    assert_eq!(
        result,
        Some(("my-server".to_string(), "my-tool".to_string()))
    );
}

#[test]
fn parse_mcp_tool_name_with_underscores() {
    let result = parse_mcp_tool_name("mcp__my_server__my_tool_v2");
    assert_eq!(
        result,
        Some(("my_server".to_string(), "my_tool_v2".to_string()))
    );
}

#[test]
fn parse_mcp_tool_name_with_double_underscore_in_tool() {
    // mcp__server__tool has trailing __ in tool name
    let result = parse_mcp_tool_name("mcp__server__list__tasks");
    // splitn(3, "__"): ["mcp", "server", "list__tasks"]
    assert_eq!(
        result,
        Some(("server".to_string(), "list__tasks".to_string()))
    );
}

#[test]
fn parse_invalid_mcp_tool_name() {
    assert_eq!(parse_mcp_tool_name("Bash"), None);
    assert_eq!(parse_mcp_tool_name("mcp__server"), None);
    assert_eq!(parse_mcp_tool_name(""), None);
}

#[test]
fn is_mcp_tool_positive() {
    assert!(is_mcp_tool("mcp__server__tool"));
    assert!(is_mcp_tool("mcp__x__y"));
}

#[test]
fn is_mcp_tool_negative() {
    assert!(!is_mcp_tool("Bash"));
    assert!(!is_mcp_tool("mcp_server_tool"));
}

#[tokio::test]
async fn manager_init_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let work = dir.path().join("project");
    std::fs::create_dir_all(&work).unwrap();
    let cfg_dir = dir.path().to_path_buf();

    let mgr = McpManager::new(work, cfg_dir);
    mgr.init().await.unwrap();

    let servers = mgr.get_all_servers().await;
    assert!(servers.is_empty());
}

#[tokio::test]
async fn manager_add_and_get_server() {
    let dir = tempfile::TempDir::new().unwrap();
    let work = dir.path().join("project");
    std::fs::create_dir_all(&work).unwrap();
    let cfg_dir = dir.path().to_path_buf();

    let mgr = McpManager::new(work.clone(), cfg_dir);

    let server = ExternalMcpServerConfig {
        name: "test-server".into(),
        transport: McpTransportType::Http,
        description: Some("A test server".into()),
        enabled: false, // don't auto-connect
        use_tools: None,
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some("http://localhost:9999".into()),
        headers: HashMap::new(),
    };

    let info = mgr
        .add_or_update(server.clone(), McpScopeType::Project)
        .await
        .unwrap();
    assert_eq!(info.config.name, "test-server");
    assert_eq!(info.status, McpServerStatus::Disconnected);
    assert_eq!(info.scope, McpScopeType::Project);

    // Should be listed
    let servers = mgr.get_all_servers().await;
    assert_eq!(servers.len(), 1);
}

#[tokio::test]
async fn manager_remove_server() {
    let dir = tempfile::TempDir::new().unwrap();
    let work = dir.path().join("project");
    std::fs::create_dir_all(&work).unwrap();
    let cfg_dir = dir.path().to_path_buf();

    let mgr = McpManager::new(work.clone(), cfg_dir);

    let server = ExternalMcpServerConfig {
        name: "to-remove".into(),
        transport: McpTransportType::Http,
        description: None,
        enabled: false,
        use_tools: None,
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some("http://localhost:9999".into()),
        headers: HashMap::new(),
    };

    mgr.add_or_update(server, McpScopeType::Project)
        .await
        .unwrap();
    assert_eq!(mgr.get_all_servers().await.len(), 1);

    let removed = mgr
        .remove("to-remove", McpScopeType::Project)
        .await
        .unwrap();
    assert!(removed);
    assert_eq!(mgr.get_all_servers().await.len(), 0);
}

#[tokio::test]
async fn manager_builtin_servers_listed() {
    let dir = tempfile::TempDir::new().unwrap();
    let work = dir.path().join("project");
    std::fs::create_dir_all(&work).unwrap();
    let cfg_dir = dir.path().to_path_buf();

    let mgr = McpManager::new(work, cfg_dir);
    let builtins = mgr.get_builtin_servers();
    // Don't pin the exact count (it grows with every new domain server) —
    // assert the core set is present and names are unique.
    assert!(builtins.len() >= 12, "got {} builtins", builtins.len());
    let names: std::collections::HashSet<_> = builtins.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names.len(), builtins.len(), "duplicate server names");
    for expected in ["senclaw-memory", "senclaw-space", "senclaw-browser"] {
        assert!(names.contains(expected), "missing {expected}");
    }
    // Each has a name starting with senclaw-
    for s in &builtins {
        assert!(
            s.name.starts_with("senclaw-"),
            "unexpected name: {}",
            s.name
        );
        assert!(!s.tools.is_empty(), "no tools for {}", s.name);
    }
}


// ===== Built-in server fallback =====
//
// SenClaw's own MCP servers are launched per chat session, so they never enter
// `external`. Anything calling from inside the daemon therefore could not reach
// them: a watch probing `dispatch_status` failed with "MCP server not found:
// senclaw-dispatch" every tick and gave up after its error streak. Found by
// running it against a live daemon, not by any unit test — these pin the seam
// that closed it.

#[tokio::test]
async fn an_unregistered_server_still_reports_not_found() {
    // The wording matters: `call_external_tool`'s error is what the watch logs
    // and what a person reads when a tool name is simply wrong.
    let dir = tempfile::TempDir::new().unwrap();
    let work = dir.path().join("project");
    std::fs::create_dir_all(&work).unwrap();
    let mgr = McpManager::new(work, dir.path().to_path_buf());

    let err = mgr
        .call_external_tool("mcp__no-such-server__thing", serde_json::json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("MCP server not found"), "got: {err}");
}

#[tokio::test]
async fn a_registered_builtin_spec_is_reachable_by_name() {
    // Registering the spec is what makes a built-in resolvable at all; without
    // it the call short-circuits to "not found" before any spawn is attempted.
    let dir = tempfile::TempDir::new().unwrap();
    let work = dir.path().join("project");
    std::fs::create_dir_all(&work).unwrap();
    let mgr = McpManager::new(work, dir.path().to_path_buf());

    mgr.register_builtin_spec(crate::mcp::helper::dispatch_mcp_config(
        &dir.path().join("dispatch-state.json").to_string_lossy(),
        "main",
        "",
        None,
    ))
    .await;

    // The spawn itself needs a real senclaw binary, which a unit test has no
    // business launching — so assert on the failure *mode*: it must no longer
    // be "server not found", i.e. the name resolved and the spawn was tried.
    let err = mgr
        .call_external_tool("mcp__senclaw-dispatch__dispatch_status", serde_json::json!({}))
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        !err.contains("MCP server not found"),
        "senclaw-dispatch must resolve once its spec is registered; got: {err}"
    );
}

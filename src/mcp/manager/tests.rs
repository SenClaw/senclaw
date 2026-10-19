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

// ---- on-demand (session-mode Space App) servers -------------------------
//
// A session app's process is not running most of the time, yet its tools have
// to stay in every new chat's roster: nothing would call a tool that is not
// offered, so the app would never start. These pin the half of that promise
// that lives in the manager.

use super::mock_server::{self, tool};

fn http_config(name: &str, url: &str) -> ExternalMcpServerConfig {
    ExternalMcpServerConfig {
        name: name.into(),
        transport: McpTransportType::Http,
        description: None,
        enabled: true,
        use_tools: None,
        command: None,
        args: vec![],
        env: HashMap::new(),
        url: Some(url.into()),
        headers: HashMap::new(),
    }
}

fn manager_in(dir: &tempfile::TempDir) -> McpManager {
    let work = dir.path().join("project");
    std::fs::create_dir_all(&work).unwrap();
    McpManager::new(work, dir.path().to_path_buf())
}

async fn roster(mgr: &McpManager) -> Vec<String> {
    let mut names: Vec<String> = mgr
        .get_external_tools_full()
        .await
        .into_iter()
        .map(|t| t.name)
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn an_app_registered_from_its_cache_has_its_tools_in_the_roster() {
    // `add_or_update_offline` documents exactly this: no connection, no process,
    // and still "tools in every agent's roster". The roster used to list only
    // servers whose status was Connected, so a stopped session app — which is
    // never Connected — contributed nothing to a chat started while it slept.
    let dir = tempfile::TempDir::new().unwrap();
    let mgr = manager_in(&dir);
    let stopped = mock_server::reserve().await;

    mgr.add_or_update_offline(
        http_config("lakehouse-mcp", &stopped.url),
        McpScopeType::Project,
        vec![tool("lake_import_file", "Import a file"), tool("lake_query", "Run SQL")],
    )
    .await
    .unwrap();

    assert_eq!(
        roster(&mgr).await,
        vec![
            "mcp__lakehouse-mcp__lake_import_file".to_string(),
            "mcp__lakehouse-mcp__lake_query".to_string(),
        ]
    );
}

#[tokio::test]
async fn a_disabled_on_demand_app_offers_nothing() {
    // The cache must not turn "off" into "on": a server the person disabled
    // keeps its cached list on disk, and none of it may reach an agent.
    let dir = tempfile::TempDir::new().unwrap();
    let mgr = manager_in(&dir);
    let stopped = mock_server::reserve().await;
    mgr.add_or_update_offline(
        http_config("lakehouse-mcp", &stopped.url),
        McpScopeType::Project,
        vec![tool("lake_query", "Run SQL")],
    )
    .await
    .unwrap();
    assert_eq!(roster(&mgr).await.len(), 1);

    mgr.update_enabled("lakehouse-mcp", McpScopeType::Project, false)
        .await
        .unwrap();
    assert!(roster(&mgr).await.is_empty());
}

#[tokio::test]
async fn a_regular_server_that_is_not_connected_offers_nothing() {
    // Only on-demand servers are listed while disconnected. A server that
    // failed to connect has no process behind it that a call could start, so
    // listing its tools would offer calls that can only fail.
    let dir = tempfile::TempDir::new().unwrap();
    let mgr = manager_in(&dir);

    // Port 1 refuses the connection at once.
    mgr.add_or_update(
        http_config("plain-mcp", "http://127.0.0.1:1/mcp"),
        McpScopeType::Project,
    )
    .await
    .unwrap();
    assert_eq!(
        mgr.get_server_info("plain-mcp").await.status,
        McpServerStatus::Error
    );

    assert!(roster(&mgr).await.is_empty());
}

#[tokio::test]
async fn an_on_demand_apps_tools_survive_the_idle_reaper_disconnecting_it() {
    // The reaper stops an idle session app and drops its MCP connection. That
    // used to empty the tool list as well, so a chat started in the next minute
    // saw none of the app's tools — the reported failure, one minute after any
    // use. Dropping the connection must not drop the roster entry.
    let dir = tempfile::TempDir::new().unwrap();
    let mgr = manager_in(&dir);
    let reserved = mock_server::reserve().await;
    let url = reserved.url.clone();
    let _running = reserved.serve(vec![tool("lake_query", "Run SQL")]);

    mgr.add_or_update_on_demand(http_config("lakehouse-mcp", &url), McpScopeType::Project)
        .await
        .unwrap();
    assert_eq!(
        mgr.get_server_info("lakehouse-mcp").await.status,
        McpServerStatus::Connected
    );
    assert_eq!(roster(&mgr).await, vec!["mcp__lakehouse-mcp__lake_query".to_string()]);

    mgr.disconnect_server("lakehouse-mcp").await.unwrap();

    let info = mgr.get_server_info("lakehouse-mcp").await;
    assert_eq!(info.status, McpServerStatus::Disconnected);
    assert_eq!(roster(&mgr).await, vec!["mcp__lakehouse-mcp__lake_query".to_string()]);
}

#[tokio::test]
async fn a_regular_server_forgets_its_tools_when_disconnected() {
    // The other side of the line above: only an on-demand app is expected to
    // come back by itself on the next call.
    let dir = tempfile::TempDir::new().unwrap();
    let mgr = manager_in(&dir);
    let reserved = mock_server::reserve().await;
    let url = reserved.url.clone();
    let _running = reserved.serve(vec![tool("plain_tool", "A tool")]);

    mgr.add_or_update(http_config("plain-mcp", &url), McpScopeType::Project)
        .await
        .unwrap();
    assert_eq!(roster(&mgr).await.len(), 1);

    mgr.disconnect_server("plain-mcp").await.unwrap();
    assert!(roster(&mgr).await.is_empty());
}

#[tokio::test]
async fn the_first_call_to_a_listed_but_stopped_app_connects_and_answers() {
    // The other half of the promise: the tool is offered while the app sleeps,
    // and calling it is what wakes the app (in the daemon, the URL is the app
    // proxy, which starts the process before forwarding).
    let dir = tempfile::TempDir::new().unwrap();
    let mgr = manager_in(&dir);
    let stopped = mock_server::reserve().await;
    let url = stopped.url.clone();

    mgr.add_or_update_offline(
        http_config("lakehouse-mcp", &url),
        McpScopeType::Project,
        vec![tool("lake_query", "Run SQL")],
    )
    .await
    .unwrap();
    assert_eq!(
        mgr.get_server_info("lakehouse-mcp").await.status,
        McpServerStatus::Disconnected
    );

    let _started = stopped.serve(vec![tool("lake_query", "Run SQL")]);
    let out = mgr
        .call_external_tool(
            "mcp__lakehouse-mcp__lake_query",
            serde_json::json!({ "path": "demo" }),
        )
        .await
        .unwrap();

    assert!(out.to_string().contains("lake_query"), "got: {out}");
    assert_eq!(
        mgr.get_server_info("lakehouse-mcp").await.status,
        McpServerStatus::Connected
    );
}

#[tokio::test]
async fn a_failed_connect_keeps_an_on_demand_apps_cached_tools() {
    // The app may fail to start for a minute (missing dependency, busy port).
    // Its tools must still be offered, or nothing could ever call it back to
    // life — the same reason an empty list never overwrites the on-disk cache.
    let dir = tempfile::TempDir::new().unwrap();
    let mgr = manager_in(&dir);
    // Port 1 refuses the connection at once.
    mgr.add_or_update_offline(
        http_config("lakehouse-mcp", "http://127.0.0.1:1/mcp"),
        McpScopeType::Project,
        vec![tool("lake_query", "Run SQL")],
    )
    .await
    .unwrap();

    let err = mgr
        .call_external_tool("mcp__lakehouse-mcp__lake_query", serde_json::json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not connected"), "got: {err}");

    assert_eq!(
        mgr.get_server_info("lakehouse-mcp").await.status,
        McpServerStatus::Error
    );
    assert_eq!(roster(&mgr).await, vec!["mcp__lakehouse-mcp__lake_query".to_string()]);
}

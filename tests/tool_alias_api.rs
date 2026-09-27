//! End-to-end test for MCP tool aliases (Plugins → Alias): boots the real UI
//! router on an ephemeral port and exercises the REST lifecycle over HTTP —
//! create → list → toggle → update → delete — against a temp SQLite DB,
//! asserting the process-wide alias registry (the one `resolve_tool_by_name`
//! consults) tracks every mutation live.

use std::sync::Arc;

use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};
use senclaw::tools::tool_alias;

fn temp_state() -> (Arc<UiState>, Arc<Db>) {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("tool-alias-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    cfg.paths.db_path = dir.join("test.db");
    // `Db::open` opens TWO files and `cognitive_db_path` is configured
    // independently — leaving it alone points this test at the developer's
    // real ~/.senclaw/senclaw_cognitive.db and runs schema migrations on it.
    cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
    let db = Arc::new(Db::open(&cfg).unwrap());
    let state = Arc::new(UiState {
        config: Arc::new(cfg),
        db: Some(Arc::clone(&db)),
        group_manager: None,
        wiki_manager: None,
        persona_registry: None,
        agent_api: None,
        mcp_manager: None,
        dispatch_bridge: None,
        marketplace_manager: None,
        workbench_bridge: None,
        space_mcp_launcher: None,
        workflow_service: None,
        virtual_worker_pool: None,
        agent_states: None,
        background_scheduler: None,
        usage_recorder: None,
        checkpoints: None,
        runtime_manager: None,
        ws_port: 0,
        ws_token: String::new(),
        // Loopback-bind posture: the token gate is off, which is what every
        // test here exercises (a request from 127.0.0.1 is always exempt).
        api_auth: Arc::new(senclaw::gateway::ui_server::auth::ApiAuth::disabled()),
    });
    (state, db)
}

#[tokio::test]
async fn alias_lifecycle_over_http() {
    let (state, db) = temp_state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();

    // ── Create a user alias (an override of a hypothetical short name) ──
    let resp = http
        .post(format!("{base}/api/tool-aliases"))
        .json(&serde_json::json!({
            "alias": "mcp__browser__navigate",
            "target": "mcp__senclaw-browser__browser_navigate",
            "description": "điều hướng"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "{}", resp.text().await.unwrap());
    // The in-process registry is refreshed immediately — no restart needed.
    assert_eq!(
        tool_alias::resolve_alias("mcp__browser__navigate").as_deref(),
        Some("mcp__senclaw-browser__browser_navigate")
    );

    // Duplicate alias → 409.
    let resp = http
        .post(format!("{base}/api/tool-aliases"))
        .json(&serde_json::json!({ "alias": "mcp__browser__navigate", "target": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);

    // Whitespace / same-name validation → 400.
    for bad in [
        serde_json::json!({ "alias": "has space", "target": "x" }),
        serde_json::json!({ "alias": "same", "target": "same" }),
        serde_json::json!({ "alias": "", "target": "x" }),
    ] {
        let resp = http
            .post(format!("{base}/api/tool-aliases"))
            .json(&bad)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400, "body: {bad}");
    }

    // ── App-declared alias (simulates a Space App manifest import) ──
    db.import_app_tool_alias(
        "ssh-manager",
        "mcp__ssh__run",
        "mcp__ssh-manager-mcp__ssh_execute_command",
        Some("chạy lệnh"),
    )
    .unwrap();

    // List shows both, app row disabled.
    let list: serde_json::Value = http
        .get(format!("{base}/api/tool-aliases"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let aliases = list["aliases"].as_array().unwrap();
    assert_eq!(aliases.len(), 2);
    let app_row = aliases
        .iter()
        .find(|a| a["source"] == "app:ssh-manager")
        .unwrap();
    assert_eq!(app_row["enabled"], false);
    assert_eq!(app_row["alias"], "mcp__ssh__run");

    // Disabled app alias must NOT resolve.
    assert_eq!(tool_alias::resolve_alias("mcp__ssh__run"), None);

    // Editing an app-managed alias is rejected — target comes from the manifest.
    let resp = http
        .put(format!("{base}/api/tool-aliases/mcp__ssh__run"))
        .json(&serde_json::json!({ "target": "mcp__evil__x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    // ── The approval gate: enable the app alias → it starts resolving ──
    let resp = http
        .post(format!("{base}/api/tool-aliases/mcp__ssh__run/enabled"))
        .json(&serde_json::json!({ "enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        tool_alias::resolve_alias("mcp__ssh__run").as_deref(),
        Some("mcp__ssh-manager-mcp__ssh_execute_command")
    );

    // ── Update the user alias target ──
    let resp = http
        .put(format!("{base}/api/tool-aliases/mcp__browser__navigate"))
        .json(&serde_json::json!({ "target": "mcp__mini-browser-mcp__mb_navigate" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        tool_alias::resolve_alias("mcp__browser__navigate").as_deref(),
        Some("mcp__mini-browser-mcp__mb_navigate")
    );

    // ── Disable the user alias → gone from the registry, still listed ──
    let resp = http
        .post(format!(
            "{base}/api/tool-aliases/mcp__browser__navigate/enabled"
        ))
        .json(&serde_json::json!({ "enabled": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(tool_alias::resolve_alias("mcp__browser__navigate"), None);

    // ── Delete ──
    let resp = http
        .delete(format!("{base}/api/tool-aliases/mcp__browser__navigate"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let resp = http
        .delete(format!("{base}/api/tool-aliases/mcp__browser__navigate"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    let list: serde_json::Value = http
        .get(format!("{base}/api/tool-aliases"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["aliases"].as_array().unwrap().len(), 1);
}

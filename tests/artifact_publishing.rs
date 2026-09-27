//! End-to-end test for Artifact Publishing: boots the real UI router on an
//! ephemeral port and exercises the full lifecycle over HTTP
//! (create → list → get → run → update → delete) against a temp SQLite DB.

use std::sync::Arc;

use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};

fn temp_state() -> Arc<UiState> {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("artifact-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    cfg.paths.db_path = dir.join("test.db");
    // `Db::open` opens TWO files and `cognitive_db_path` is configured
    // independently — leaving it alone points this test at the developer's
    // real ~/.senclaw/senclaw_cognitive.db and runs schema migrations on it.
    cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
    let db = Db::open(&cfg).unwrap();
    Arc::new(UiState {
        config: Arc::new(cfg),
        db: Some(Arc::new(db)),
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
    })
}

#[tokio::test]
async fn artifact_lifecycle_over_http() {
    // Bash artifacts run via the brush child process → point it at the real bin.
    std::env::set_var("SENCLAW_BIN", env!("CARGO_BIN_EXE_senclaw"));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(temp_state());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();

    // ── Create ──
    let created: serde_json::Value = http
        .post(format!("{base}/api/code/artifacts"))
        .json(&serde_json::json!({
            "name": "adder",
            "language": "bash",
            "code": "echo $((2 + 3))",
            "description": "adds two numbers",
            "tags": ["math"]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["name"], "adder");
    assert_eq!(created["language"], "bash");

    // ── List ──
    let list: serde_json::Value = http
        .get(format!("{base}/api/code/artifacts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = list["artifacts"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["tags"][0], "math");

    // ── Get one ──
    let got: serde_json::Value = http
        .get(format!("{base}/api/code/artifacts/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["code"], "echo $((2 + 3))");

    // ── Run (brush sandbox via child process) ──
    // Generous timeout on purpose: the child here is the freshly-built DEBUG
    // senclaw binary, whose cold start alone can blow the 5s default on a
    // loaded machine or a 2-core CI runner (this exact test was red in CI
    // for weeks because of it).
    let run: serde_json::Value = http
        .post(format!("{base}/api/code/artifacts/{id}/run"))
        .json(&serde_json::json!({ "timeout_ms": 60_000 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(run["ok"], true, "run outcome: {run}");
    assert!(
        run["result"].as_str().unwrap_or_default().contains('5'),
        "expected '5' in result, got: {run}"
    );

    // ── Update ──
    let upd = http
        .put(format!("{base}/api/code/artifacts/{id}"))
        .json(&serde_json::json!({
            "name": "adder-v2",
            "language": "bash",
            "code": "echo done",
            "description": ""
        }))
        .send()
        .await
        .unwrap();
    assert!(upd.status().is_success());
    let got2: serde_json::Value = http
        .get(format!("{base}/api/code/artifacts/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got2["name"], "adder-v2");

    // ── Delete ──
    let del = http
        .delete(format!("{base}/api/code/artifacts/{id}"))
        .send()
        .await
        .unwrap();
    assert!(del.status().is_success());
    let after = http
        .get(format!("{base}/api/code/artifacts/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(after.status().as_u16(), 404, "should be gone after delete");
}

#[tokio::test]
async fn rejects_bad_input() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(temp_state());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();

    // Missing name.
    let r = http
        .post(format!("{base}/api/code/artifacts"))
        .json(&serde_json::json!({ "name": "", "language": "bash", "code": "echo hi" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);

    // Unsupported language.
    let r = http
        .post(format!("{base}/api/code/artifacts"))
        .json(&serde_json::json!({ "name": "x", "language": "cobol", "code": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
}

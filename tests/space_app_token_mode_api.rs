//! End-to-end test for the app-isolation switch the UI drives
//! (`/api/space/app-token-mode`, Settings → Space Apps in both the web and
//! desktop clients).
//!
//! Boots the real router over HTTP because the two things most likely to be
//! wrong are invisible to a unit test: whether the route is reachable at all —
//! it sits one path segment away from `/api/space/apps/:id`, which the per-app
//! token middleware claims — and whether a change actually takes effect on the
//! very next request, which is the entire reason the mode lives in the
//! database instead of the environment.

use std::sync::Arc;

use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};

fn temp_state() -> Arc<UiState> {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("app-token-mode-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    cfg.paths.db_path = dir.join("test.db");
    // `Db::open` opens TWO files and `cognitive_db_path` is configured
    // independently — leaving it alone runs migrations on the developer's real
    // ~/.senclaw/senclaw_cognitive.db.
    cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
    // Pin the starting point rather than inheriting whatever the developer has
    // exported, so "the UI choice wins" is a real assertion.
    cfg.space_app_token_mode = senclaw::apps::token::TokenMode::Warn;
    cfg.space_app_token_mode_from_env = true;
    let db = Arc::new(Db::open(&cfg).unwrap());
    Arc::new(UiState {
        config: Arc::new(cfg),
        db: Some(db),
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
        api_auth: Arc::new(senclaw::gateway::ui_server::auth::ApiAuth::disabled()),
    })
}

async fn serve() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(temp_state());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn the_switch_reads_writes_and_reverts() {
    let base = serve().await;
    let http = reqwest::Client::new();
    let url = format!("{base}/api/space/app-token-mode");

    // Reachable, and honest about where the current value came from — the UI
    // needs `source` to know whether it can offer a way back.
    let v: serde_json::Value = http.get(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["mode"], "warn");
    assert_eq!(v["source"], "env");
    assert_eq!(v["envMode"], "warn");
    assert_eq!(v["envSet"], true);

    // A choice wins over the environment, and the PUT answers with the new
    // state so the client needs no second round-trip.
    let v: serde_json::Value = http
        .put(&url)
        .json(&serde_json::json!({ "mode": "strict" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["mode"], "strict");
    assert_eq!(v["source"], "ui");

    // It survives being read back — this is what "no restart needed" means.
    let v: serde_json::Value = http.get(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["mode"], "strict");
    assert_eq!(v["source"], "ui");

    // Handing the decision back restores the environment's answer rather than
    // freezing the last choice.
    let v: serde_json::Value = http
        .put(&url)
        .json(&serde_json::json!({ "mode": null }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["mode"], "warn");
    assert_eq!(v["source"], "env");
}

#[tokio::test]
async fn an_unknown_mode_is_refused_rather_than_coerced() {
    let base = serve().await;
    let http = reqwest::Client::new();
    let url = format!("{base}/api/space/app-token-mode");

    // Coercing a typo would set an isolation level the operator did not ask
    // for while showing them the one they typed.
    let resp = http
        .put(&url)
        .json(&serde_json::json!({ "mode": "of" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let v: serde_json::Value = http.get(&url).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["mode"], "warn", "the refused write must not have landed");
}

#[tokio::test]
async fn the_switch_is_not_swallowed_by_the_per_app_token_routes() {
    // `/api/space/app-token-mode` sits beside `/api/space/apps/:id`. If it ever
    // gets moved under `/apps/`, `app_auth` would read "app-token-mode" as an
    // app id and gate the fleet-wide switch behind one app's token — which,
    // under strict mode, would lock the operator out of the control that turns
    // strict mode off.
    assert!(
        senclaw::gateway::ui_server::app_auth::split_app_path("/api/space/app-token-mode")
            .is_none()
    );
}

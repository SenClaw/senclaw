//! The readiness gate the UI opens an app through.
//!
//! A server Space App is its own process on its own port, and both frontends
//! point a web view straight at that origin. Pointed at a stopped app that is a
//! blank white rectangle with no error in it — and since apps gained a
//! `session` mode, *stopped is the resting state* for most of them, so the
//! blank window went from rare to routine.
//!
//! `GET /api/space/apps/:id/ready` is what the UI asks before it mounts
//! anything. What it must never do is answer "ready" for an app that merely has
//! a pid, or 404 for an app the daemon simply hasn't started — both send the UI
//! back to rendering white.

use std::sync::Arc;

use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};

fn temp_state() -> (Arc<UiState>, Arc<Db>, std::path::PathBuf) {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("ready-gate-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    cfg.paths.db_path = dir.join("test.db");
    // `Db::open` opens TWO files and `cognitive_db_path` is configured
    // independently — leaving it alone points this test at the developer's
    // real ~/.senclaw/senclaw_cognitive.db and runs schema migrations on it.
    cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
    let db_path = cfg.paths.db_path.clone();
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
        api_auth: Arc::new(senclaw::gateway::ui_server::auth::ApiAuth::disabled()),
    });
    (state, db, db_path)
}

fn install_app(db_path: &std::path::Path, id: &str, manifest: serde_json::Value) {
    // `Db::with_conn` is crate-private, so seed over a second connection to the
    // same file — the daemon's own schema is already there.
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO space_apps (id, manifest, enabled, installed_at) \
         VALUES (?1, ?2, 1, 0)",
        rusqlite::params![id, manifest.to_string()],
    )
    .unwrap();
}

async fn serve() -> (String, std::path::PathBuf) {
    let (state, _db, db_path) = temp_state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), db_path)
}

#[tokio::test]
async fn a_stopped_server_app_is_not_ready_but_still_answers() {
    let (base, db) = serve().await;
    install_app(
        &db,
        "ghost",
        serde_json::json!({
            "id": "ghost",
            "runtime": { "kind": "server", "start": "./ghost", "port": 4999, "healthPath": "/api/status" }
        }),
    );

    let resp = reqwest::get(format!("{base}/api/space/apps/ghost/ready"))
        .await
        .unwrap();
    // 200 with ready:false, NOT an error. "Not running" is the resting state of
    // a session app; a 4xx/5xx here would read to the UI as a broken install
    // and it would have nothing useful to show.
    assert_eq!(resp.status().as_u16(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["ready"], false, "nothing is listening on 4999");
    assert_eq!(v["running"], false);
    assert_eq!(v["kind"], "server");
    assert_eq!(v["port"], 4999);
    // The UI needs somewhere to point once it *is* ready.
    assert_eq!(v["proxyUrl"], "/api/space/apps/ghost/proxy/");
}

#[tokio::test]
async fn a_static_app_is_always_ready() {
    let (base, db) = serve().await;
    install_app(
        &db,
        "docs",
        serde_json::json!({ "id": "docs", "runtime": { "kind": "static" } }),
    );

    let v: serde_json::Value = reqwest::get(format!("{base}/api/space/apps/docs/ready"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // A static app is served by the daemon itself. Gating it on a health check
    // it can never pass would strand every static and esm app behind a door
    // with no handle.
    assert_eq!(v["ready"], true);
    assert_eq!(v["kind"], "static");
}

#[tokio::test]
async fn ready_reports_the_app_that_is_actually_answering() {
    let (base, db) = serve().await;

    // Stand in for the app: something that really does answer on its port.
    let app = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = app.local_addr().unwrap().port();
    tokio::spawn(async move {
        let router = axum::Router::new().route(
            "/api/status",
            axum::routing::get(|| async { axum::Json(serde_json::json!({ "ok": true })) }),
        );
        axum::serve(app, router).await.unwrap();
    });

    install_app(
        &db,
        "live",
        serde_json::json!({
            "id": "live",
            "runtime": { "kind": "server", "start": "./live", "port": port, "healthPath": "/api/status" }
        }),
    );

    let v: serde_json::Value = reqwest::get(format!("{base}/api/space/apps/live/ready"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // No launcher in this state, so `running` (do we track a pid?) is false —
    // and that is precisely why the UI must gate on `ready`, which asks the
    // port rather than the bookkeeping.
    assert_eq!(v["ready"], true, "the port answers: {v}");
    assert_eq!(v["running"], false);
}

#[tokio::test]
async fn ready_on_an_unknown_app_is_a_404() {
    let (base, _db) = serve().await;
    let resp = reqwest::get(format!("{base}/api/space/apps/nope/ready"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

//! The fleet lifecycle snapshot an agent reads before it starts or stops an app.
//!
//! `/api/space/apps` says what is installed and `/api/space/apps/:id/runtime`
//! says what one process is doing. Between them nothing answered "which of my
//! fifty apps are up" without fifty requests — which is the question the
//! `space_app_list` MCP tool asks on every turn of a conversation, so it gets
//! one endpoint and that endpoint must stay cheap.
//!
//! The traps this pins down: `status` is a literal sibling of `:id` in the
//! router, so anything that treats it as an app id (`app_auth`) turns the fleet
//! listing into a scoped call against an app that does not exist; and a static
//! app has no process at all, so reporting it as "not running" would have an
//! agent try to start something the daemon already serves.

use std::sync::Arc;

use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};

fn temp_state() -> (Arc<UiState>, Arc<Db>, std::path::PathBuf) {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("app-status-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    cfg.paths.db_path = dir.join("test.db");
    // `Db::open` opens TWO files; leaving the cognitive path alone would run
    // migrations against the developer's real ~/.senclaw database.
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
        // A real launcher, not None: the interesting rows are the ones that ask
        // it whether an app is running, user-stopped, or has been relaunched.
        space_mcp_launcher: Some(Arc::new(
            senclaw::gateway::ui_server::space_mcp::SpaceMcpLauncher::new(),
        )),
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

/// The three shapes an installed app can have, seeded together so one listing
/// has to get all three right.
fn install_fleet(db: &std::path::Path) {
    install_app(
        db,
        "docs",
        serde_json::json!({ "id": "docs", "name": "Docs", "runtime": { "kind": "static" } }),
    );
    install_app(
        db,
        "kanban",
        serde_json::json!({
            "id": "kanban",
            "name": "Kanban",
            "runtime": {
                "kind": "server", "start": "./kanban", "port": 4340,
                "mode": "session", "idleTimeoutSecs": 90,
            },
            "mcp": { "name": "kanban-mcp", "path": "/mcp" },
        }),
    );
    install_app(
        db,
        "watcher",
        serde_json::json!({
            "id": "watcher",
            "name": "Watcher",
            "runtime": { "kind": "server", "start": "./watcher", "port": 4341, "mode": "background" },
            // No `mcp.name`: the server is `<id>-mcp` by convention, and a row
            // that reported `null` here would have an agent conclude the app
            // exposes no tools.
            "mcp": { "path": "/mcp" },
        }),
    );
}

fn find<'a>(v: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    v["apps"]
        .as_array()
        .expect("apps array")
        .iter()
        .find(|a| a["id"] == id)
        .unwrap_or_else(|| panic!("no row for {id} in {v}"))
}

#[tokio::test]
async fn one_call_reports_the_whole_fleet() {
    let (base, db) = serve().await;
    install_fleet(&db);

    let resp = reqwest::get(format!("{base}/api/space/apps/status"))
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "`status` is a literal sibling of `:id`; a 4xx here means something \
         parsed it as an app id"
    );
    let v: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(v["total"], 3);
    assert_eq!(
        v["serverApps"], 2,
        "only the two server apps have a process"
    );
    assert_eq!(v["running"], 0, "nothing was launched");
    assert_eq!(v["probed"], false, "a probe costs a round-trip per app");

    // Static: no process, and saying so as "not running" would have an agent
    // try to start something the daemon already serves.
    let docs = find(&v, "docs");
    assert_eq!(docs["kind"], "static");
    assert_eq!(docs["mode"], "none");
    assert_eq!(docs["running"], true);

    // Session server app, with everything start/stop needs.
    let kanban = find(&v, "kanban");
    assert_eq!(kanban["name"], "Kanban");
    assert_eq!(kanban["kind"], "server");
    assert_eq!(kanban["mode"], "session");
    assert_eq!(kanban["port"], 4340);
    assert_eq!(kanban["running"], false);
    assert_eq!(kanban["userStopped"], false);
    assert_eq!(kanban["launches"], 0);
    assert_eq!(kanban["idleTimeoutSecs"], 90);
    assert_eq!(kanban["mcpName"], "kanban-mcp");
    assert_eq!(
        kanban["ready"],
        serde_json::Value::Null,
        "no probe was asked for, so readiness is unknown — not false"
    );

    assert_eq!(find(&v, "watcher")["mode"], "background");
}

/// The manifest is the source of truth for an MCP server's name, and only
/// *sometimes* spells it out. Deriving `<id>-mcp` in one place and reading
/// `mcp.name` in another is how luna-calendar (server `luna-mcp`) ends up
/// listed under a server that does not exist.
#[tokio::test]
async fn an_undeclared_mcp_name_falls_back_to_the_convention() {
    let (base, db) = serve().await;
    install_fleet(&db);

    let v: serde_json::Value = reqwest::get(format!("{base}/api/space/apps/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(find(&v, "watcher")["mcpName"], "watcher-mcp");
    assert_eq!(
        find(&v, "docs")["mcpName"],
        serde_json::Value::Null,
        "an app with no `mcp` block exposes no tools; naming a server for it \
         would imply an agent could call into it"
    );
}

/// `running` is bookkeeping and `ready` is a health probe. Conflating them is
/// what renders a white window: a tracked pid that has not bound its port yet
/// reads as "up".
#[tokio::test]
async fn probe_asks_the_port_and_the_default_does_not() {
    let (base, db) = serve().await;
    install_fleet(&db);

    let v: serde_json::Value = reqwest::get(format!("{base}/api/space/apps/status?probe=1"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(v["probed"], true);
    let kanban = find(&v, "kanban");
    assert_eq!(
        kanban["ready"], false,
        "nothing is listening on 4340, and the probe must say so rather than \
         inherit `running`"
    );
    assert_eq!(kanban["running"], false);

    // A static app is not probed at all — it has no port to ask.
    assert!(find(&v, "docs").get("ready").is_none());
}

/// An empty install is a legitimate state, not an error: a fresh machine has no
/// apps and an agent asking "what is installed" must get an answer it can read.
#[tokio::test]
async fn an_empty_install_is_an_empty_list_not_an_error() {
    let (base, _db) = serve().await;

    let resp = reqwest::get(format!("{base}/api/space/apps/status"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["total"], 0);
    assert_eq!(v["apps"].as_array().unwrap().len(), 0);
}

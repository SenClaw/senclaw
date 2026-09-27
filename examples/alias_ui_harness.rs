//! Manual test harness for the Plugins → Alias UI.
//!
//! Boots the real UI router (new `/api/tool-aliases` REST + the built
//! `web/dist` SPA) on `127.0.0.1:18988` against an isolated temp SQLite DB
//! seeded with one user alias and one app-declared alias — so the panel can
//! be exercised end-to-end without touching the running desktop daemon or
//! the real `~/.senclaw` data.
//!
//! Run:
//!   npm run build:web            # the harness serves web/dist
//!   cargo run --example alias_ui_harness
//! then open http://127.0.0.1:18988/plugins?nav=alias

use std::sync::Arc;

use senclaw::config::Config;
use senclaw::db::tool_aliases::SOURCE_USER;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};

#[tokio::main]
async fn main() {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("alias-ui-harness-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create harness dir");
    cfg.paths.db_path = dir.join("harness.db");

    let db = Arc::new(Db::open(&cfg).expect("open harness db"));
    let _ = db.create_tool_alias(
        "mcp__browser__navigate",
        "mcp__senclaw-browser__browser_navigate",
        Some("Tên ngắn cho browser_navigate"),
        true,
        SOURCE_USER,
    );
    let _ = db.import_app_tool_alias(
        "ssh-manager",
        "mcp__ssh__run",
        "mcp__ssh-manager-mcp__ssh_execute_command",
        Some("Chạy lệnh trên host đã lưu"),
    );
    senclaw::tools::tool_alias::reload_from_db(&db);

    let state = Arc::new(UiState {
        config: Arc::new(cfg),
        db: Some(db),
        group_manager: None,
        wiki_manager: None,
        persona_registry: None,
        agent_api: None,
        mcp_manager: None,
        marketplace_manager: None,
        workbench_bridge: None,
        dispatch_bridge: None,
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
        // The harness binds loopback, which is exactly the case the daemon
        // itself leaves unauthenticated — no token to fetch before poking the
        // alias page by hand.
        api_auth: Arc::new(senclaw::gateway::ui_server::auth::ApiAuth::disabled()),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:18988")
        .await
        .expect("bind 18988");
    println!("alias UI harness → http://127.0.0.1:18988/plugins?nav=alias");
    axum::serve(listener, build_router(state)).await.unwrap();
}

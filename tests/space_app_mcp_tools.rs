//! The `space_app_*` MCP tools, driven against a real daemon router.
//!
//! These are the tools a chat agent uses to see, start and stop Space Apps. All
//! four go out over loopback HTTP because the thing they manipulate — the
//! daemon's `SpaceMcpLauncher` — is a child-process map in another process, not
//! a row in the shared database. That makes the interesting failures *wiring*
//! failures, which no unit test on the tool body can catch: a renamed route, a
//! filter applied to the wrong field, an id interpolated straight into a path.
//!
//! So the test stands up the actual router and calls the actual tool bodies.

use std::sync::Arc;

use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};
use senclaw::mcp::space_apps::SpaceAppsClient;

/// `with_mcp` decides whether the daemon has an MCP registry at all — the
/// difference between the join working and the tool having to degrade.
fn temp_state(with_mcp: bool) -> (Arc<UiState>, std::path::PathBuf) {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("app-tools-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    cfg.paths.db_path = dir.join("test.db");
    cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
    let db_path = cfg.paths.db_path.clone();
    let db = Arc::new(Db::open(&cfg).unwrap());
    let state = Arc::new(UiState {
        config: Arc::new(cfg),
        db: Some(db),
        group_manager: None,
        wiki_manager: None,
        persona_registry: None,
        agent_api: None,
        dispatch_bridge: None,
        mcp_manager: with_mcp.then(|| {
            Arc::new(senclaw::mcp::manager::McpManager::new(
                dir.clone(),
                dir.clone(),
            ))
        }),
        marketplace_manager: None,
        workbench_bridge: None,
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
    (state, db_path)
}

fn install_app(db_path: &std::path::Path, id: &str, manifest: serde_json::Value) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO space_apps (id, manifest, enabled, installed_at) \
         VALUES (?1, ?2, 1, 0)",
        rusqlite::params![id, manifest.to_string()],
    )
    .unwrap();
}

/// A daemon plus a client pointed at it — the pair every test here needs.
async fn daemon() -> (SpaceAppsClient, std::path::PathBuf) {
    daemon_with(true).await
}

async fn daemon_with(mcp: bool) -> (SpaceAppsClient, std::path::PathBuf) {
    let (state, db_path) = temp_state(mcp);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (
        SpaceAppsClient::with_base_url(format!("http://{addr}")),
        db_path,
    )
}

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
            "name": "Kanban Board",
            "runtime": { "kind": "server", "start": "./k", "port": 4340, "mode": "session" },
            "mcp": { "name": "kanban-mcp", "path": "/mcp" },
        }),
    );
    install_app(
        db,
        "watcher",
        serde_json::json!({
            "id": "watcher",
            "name": "Watcher",
            "runtime": { "kind": "server", "start": "./w", "port": 4341, "mode": "background" },
        }),
    );
}

fn json(result: &senclaw::mcp::schedule_server::ToolResult) -> serde_json::Value {
    assert!(!result.is_error, "tool errored: {}", result.content);
    serde_json::from_str(&result.content)
        .unwrap_or_else(|e| panic!("tool returned non-JSON ({e}): {}", result.content))
}

#[tokio::test]
async fn list_reports_every_installed_app() {
    let (client, db) = daemon().await;
    install_fleet(&db);

    let v = json(&client.list(None, None, false).await);
    assert_eq!(v["count"], 3);
    assert_eq!(v["filter"], "all");
    // The legend is what stops an agent reading "session app not running" as a
    // fault and restarting something that is working exactly as designed.
    assert!(v["legend"]["mode"].as_str().unwrap().contains("session"));
}

#[tokio::test]
async fn list_filters_by_name_or_id_case_insensitively() {
    let (client, db) = daemon().await;
    install_fleet(&db);

    let by_id = json(&client.list(Some("KANBAN".into()), None, false).await);
    assert_eq!(by_id["count"], 1);
    assert_eq!(by_id["apps"][0]["id"], "kanban");

    // Matching the display name too: a user says "the kanban board", not an id.
    let by_name = json(&client.list(Some("board".into()), None, false).await);
    assert_eq!(by_name["count"], 1);
    assert_eq!(by_name["apps"][0]["id"], "kanban");

    let miss = json(
        &client
            .list(Some("nothing-like-this".into()), None, false)
            .await,
    );
    assert_eq!(miss["count"], 0);
}

#[tokio::test]
async fn a_misspelled_status_filter_shows_everything_rather_than_nothing() {
    let (client, db) = daemon().await;
    install_fleet(&db);

    // Only the static app counts as running here — nothing was launched.
    let running = json(&client.list(None, Some("running".into()), false).await);
    assert_eq!(running["count"], 1);
    assert_eq!(running["apps"][0]["id"], "docs");

    let stopped = json(&client.list(None, Some("stopped".into()), false).await);
    assert_eq!(stopped["count"], 2);

    // An agent that asked for "up" and got an empty list would report that the
    // machine has no apps. Falling back to everything is wrong-but-visible;
    // falling back to a filter is wrong-and-silent.
    let typo = json(&client.list(None, Some("up".into()), false).await);
    assert_eq!(typo["count"], 3);
}

#[tokio::test]
async fn mcp_list_joins_apps_to_their_servers_and_skips_apps_without_one() {
    let (client, db) = daemon().await;
    install_fleet(&db);

    let v = json(&client.mcp_list(None, None).await);
    // `docs` declares no `mcp` block, so it must not appear: listing it would
    // imply an agent could call tools into it.
    assert_eq!(v["count"], 1);
    let row = &v["apps"][0];
    assert_eq!(row["appId"], "kanban");
    assert_eq!(row["mcpName"], "kanban-mcp");
    assert_eq!(
        row["registered"], false,
        "nothing has registered `kanban-mcp` with this daemon — and the tool \
         must say so rather than omit the app"
    );
    assert_eq!(row["toolCount"], 0);
    // The whole point of the tool: how to actually call one of these.
    assert_eq!(v["callFormat"], "mcp__<mcpName>__<tool>");
    assert!(v.get("registryError").is_none(), "the registry answered");
}

/// An unreadable MCP registry must not take the whole answer down with it.
///
/// Which server an app registers comes from its manifest, and that is the half
/// a caller most needs — the name to call a tool by. Only live status and the
/// tool list come from the registry. Erroring out would withhold the good half
/// over the missing one; returning `registered: false` would be worse still,
/// asserting something that was never checked.
#[tokio::test]
async fn an_unreadable_registry_degrades_instead_of_failing() {
    let (client, db) = daemon_with(false).await;
    install_fleet(&db);

    let r = client.mcp_list(None, None).await;
    assert!(!r.is_error, "degraded, not failed: {}", r.content);
    let v: serde_json::Value = serde_json::from_str(&r.content).unwrap();

    assert_eq!(v["count"], 1);
    assert_eq!(
        v["apps"][0]["mcpName"], "kanban-mcp",
        "still from the manifest"
    );
    assert_eq!(
        v["apps"][0]["registered"],
        serde_json::Value::Null,
        "an unread registry is not evidence of an unregistered server"
    );
    // Loud, because `toolCount: 0` is otherwise indistinguishable from an app
    // that genuinely exposes nothing.
    assert!(v["registryError"].is_string(), "{v}");
    assert!(v["degraded"].is_string(), "{v}");
}

#[tokio::test]
async fn mcp_list_for_one_app_includes_tool_names_and_rejects_unknown_ids() {
    let (client, db) = daemon().await;
    install_fleet(&db);

    let one = json(&client.mcp_list(Some("kanban".into()), None).await);
    assert_eq!(one["count"], 1);
    assert!(
        one["apps"][0]["tools"].is_array(),
        "asking about one app implies wanting its tool names"
    );

    // Fleet-wide, names are omitted by default — several hundred entries would
    // bury the answer.
    let all = json(&client.mcp_list(None, None).await);
    assert!(all["apps"][0].get("tools").is_none());
    // …unless asked for.
    let forced = json(&client.mcp_list(None, Some(true)).await);
    assert!(forced["apps"][0]["tools"].is_array());

    let missing = client.mcp_list(Some("no-such-app".into()), None).await;
    assert!(
        missing.is_error,
        "an unknown id must not return an empty list"
    );
    assert!(
        missing.content.contains("no-such-app"),
        "{}",
        missing.content
    );
}

/// The id lands in a URL path, so a separator in it would aim the POST at a
/// route the caller never named. Rejected before any request goes out.
#[tokio::test]
async fn a_malformed_app_id_never_reaches_the_daemon() {
    let (client, db) = daemon().await;
    install_fleet(&db);

    for bad in ["../../api/space/apps", "kanban/stop", "kanban%2Fstop", ""] {
        for r in [
            client.start(bad).await,
            client.stop(bad).await,
            client.restart(bad).await,
        ] {
            assert!(r.is_error, "`{bad}` was accepted");
            assert!(
                r.content.contains("app_id"),
                "the message must name the offending parameter: {}",
                r.content
            );
        }
    }
}

/// Stopping says what "stopped" *means*, and it differs by mode: a session app
/// comes back by itself, a background one does not. An agent that reports the
/// wrong one leaves the user thinking a channel is still being watched.
#[tokio::test]
async fn stop_explains_what_it_did_per_mode() {
    let (client, db) = daemon().await;
    install_fleet(&db);

    let session = json(&client.stop("kanban").await);
    assert_eq!(session["success"], true);
    assert_eq!(session["mode"], "session");
    assert_eq!(session["wasRunning"], false);
    assert!(
        session["note"].as_str().unwrap().contains("by itself"),
        "{}",
        session["note"]
    );

    let background = json(&client.stop("watcher").await);
    assert_eq!(background["mode"], "background");
    assert!(
        background["note"]
            .as_str()
            .unwrap()
            .contains("stays stopped"),
        "{}",
        background["note"]
    );

    // And the stop sticks: the supervisor must not put a background app back up
    // within a tick, which would read as the tool not working.
    let after = json(&client.list(Some("watcher".into()), None, false).await);
    assert_eq!(after["apps"][0]["userStopped"], true);
}

#[tokio::test]
async fn an_uninstalled_app_is_a_clear_error_not_a_silent_success() {
    let (client, _db) = daemon().await;

    let r = client.stop("ghost").await;
    assert!(r.is_error);
    assert!(
        r.content.contains("404") || r.content.contains("not found"),
        "{}",
        r.content
    );
}

/// The overwhelmingly likely failure in the field is not a bug in the call but
/// a daemon that is not running. "error sending request" alone sends the agent
/// hunting in the wrong place.
#[tokio::test]
async fn a_dead_daemon_says_so_and_names_the_address() {
    // Port 1 on loopback: nothing binds it, and the connection refusal is
    // immediate rather than a timeout.
    let client = SpaceAppsClient::with_base_url("http://127.0.0.1:1");

    let r = client.list(None, None, false).await;
    assert!(r.is_error);
    assert!(r.content.contains("127.0.0.1:1"), "{}", r.content);
    assert!(r.content.contains("SENCLAW_SPACE_API_URL"), "{}", r.content);
}

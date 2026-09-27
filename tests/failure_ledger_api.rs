//! The failure ledger end to end: engine events in, rows out over real HTTP.
//!
//! Two classes of bug live here and nowhere else. The event path is
//! asynchronous — `record` maps an event to an op, a worker applies it — so a
//! unit test of either half can pass while the pair records nothing. And a
//! `Query` field's name *is* its wire name: `/api/watches` shipped with
//! `chatJid` rejected as a 400 and its UI strip silently never loaded.

use std::sync::Arc;
use std::time::Duration;

use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};
use senclaw::zen_core::{
    EngineEvent, InputReceivedData, MessageCompleteData, ToolExecutionCompleteData,
    ToolExecutionErrorData,
};

const JID: &str = "web:code:ledger";

async fn serve() -> (String, Arc<Db>) {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("failure-ledger-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    cfg.paths.db_path = dir.join("test.db");
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
        api_auth: Arc::new(senclaw::gateway::ui_server::auth::ApiAuth::disabled()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), db)
}

fn tool_failed(tool: &str, content: &str) -> EngineEvent {
    let mut shape = std::collections::BTreeMap::new();
    shape.insert("host".to_string(), "string:empty".to_string());
    shape.insert("port".to_string(), "number".to_string());
    EngineEvent::ToolExecutionError(ToolExecutionErrorData {
        agent_id: "main".into(),
        tool_name: tool.into(),
        title: tool.into(),
        description: String::new(),
        content: content.into(),
        args_shape: shape,
    })
}

fn tool_ok(tool: &str) -> EngineEvent {
    EngineEvent::ToolExecutionComplete(ToolExecutionCompleteData {
        agent_id: "main".into(),
        tool_name: tool.into(),
        title: String::new(),
        summary: String::new(),
        description: String::new(),
        content: serde_json::json!({}),
    })
}

fn user_said_something() -> EngineEvent {
    EngineEvent::InputReceived(InputReceivedData {
        input: "the host is 10.0.0.4".into(),
        queued: false,
        inject: false,
        queue_length: 0,
    })
}

fn agent_answered() -> EngineEvent {
    EngineEvent::MessageComplete(MessageCompleteData {
        agent_id: "main".into(),
        reasoning: String::new(),
        content: "I could not connect — what is the host?".into(),
        has_tool_calls: false,
        tool_calls: None,
        output_tokens: 0,
    })
}

async fn get(base: &str, tail: &str) -> (u16, serde_json::Value) {
    let r = reqwest::get(format!("{base}{tail}")).await.unwrap();
    let status = r.status().as_u16();
    let body = r.json::<serde_json::Value>().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// Poll until the ledger's async worker has caught up (or give up).
async fn settle<F>(mut done: F)
where
    F: FnMut() -> bool,
{
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("ledger never caught up");
}

/// The whole path in one test: `failures::start` installs a process-global
/// sender, so a second test in this binary would write into the first test's
/// database.
#[tokio::test]
async fn a_failure_the_user_fixes_is_recorded_and_served() {
    let (base, db) = serve().await;
    senclaw::failures::start(Arc::clone(&db));

    // Two failing calls, the agent gives up and asks, the user answers, the
    // next call works. This is the sequence the whole ledger exists to count.
    senclaw::failures::record(JID, &tool_failed("ssh_start_connect", "Missing host, port, or user"));
    senclaw::failures::record(JID, &tool_failed("ssh_start_connect", "Missing host, port, or user"));
    senclaw::failures::record(JID, &agent_answered());
    senclaw::failures::record(JID, &user_said_something());
    senclaw::failures::record(JID, &tool_ok("ssh_start_connect"));

    let probe = Arc::clone(&db);
    settle(move || {
        probe
            .list_failure_episodes(Some(JID), Some("resolved"), 10)
            .map(|r| !r.is_empty())
            .unwrap_or(false)
    })
    .await;

    // camelCase on the wire, and the chat filter actually filters.
    let (status, body) = get(
        &base,
        &format!("/api/failures?chatJid={}", urlencoding::encode(JID)),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["count"], 1, "{body}");
    let ep = &body["episodes"][0];
    assert_eq!(ep["toolName"], "ssh_start_connect");
    assert_eq!(ep["status"], "resolved");
    assert_eq!(ep["fixSource"], "user", "a user message preceded the fix");
    assert_eq!(ep["streak"], 2, "both failures are one episode");
    assert_eq!(ep["turnsEnded"], 1);
    // Shape, never values.
    assert_eq!(ep["argsShape"]["host"], "string:empty");
    assert!(
        !body.to_string().contains("10.0.0.4"),
        "the user's message must not reach the ledger: {body}"
    );

    let (status, sum) = get(&base, "/api/failures/summary?days=1").await;
    assert_eq!(status, 200, "{sum}");
    assert_eq!(sum["episodes"], 1);
    assert_eq!(sum["byFixSource"]["user"], 1);
    assert_eq!(sum["repeatRate"], 0.0, "one episode cannot repeat");
    assert_eq!(sum["topFailures"][0]["tool"], "ssh_start_connect");

    // An unknown chat is an empty list, not an error.
    let (status, body) = get(&base, "/api/failures?chatJid=web:nobody").await;
    assert_eq!(status, 200);
    assert_eq!(body["count"], 0);
}

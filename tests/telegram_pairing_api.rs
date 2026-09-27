//! `/api/pairings` over real HTTP.
//!
//! The unit tests in `gateway::pairing` prove the policy; they cannot catch the
//! class of bug this file exists for. Both of the pairing-adjacent bugs this
//! codebase has already shipped were invisible to `cargo check` *and* to unit
//! tests, and were found by calling the running daemon: a `Query<T>` field
//! whose name silently *is* the wire name (every client's param rejected with a
//! 400), and an endpoint that reported success for an update that matched no
//! rows. So: route registration, param spelling, status codes and JSON shapes
//! are asserted against an actual axum server here.

use std::sync::Arc;

use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};

fn temp_state() -> (Arc<UiState>, Arc<Db>) {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("pairing-api-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    cfg.paths.db_path = dir.join("test.db");
    // `Db::open` opens two files; leaving the cognitive path alone would run
    // migrations on the developer's real graph.
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
    (state, db)
}

/// A channel with an agent attached and a binding still waiting for its chat —
/// exactly what "Add Agent" in the web UI leaves behind.
fn seed(db: &Db) -> i64 {
    let now = "2026-09-08T00:00:00Z";
    let ch = db
        .insert_channel("telegram", "TG", r#"{"botToken":"bot-A"}"#, now)
        .unwrap();
    let agent = db
        .insert_agent("main", "Main", false, None, None, "", None, now)
        .unwrap();
    db.insert_binding(None, agent, ch, None, None, now).unwrap();
    ch
}

fn request_for(db: &Db, channel_id: i64, chat_jid: &str) -> senclaw::types::ChannelPairing {
    let channel = db.get_channel(channel_id).unwrap().unwrap();
    let msg = senclaw::types::IncomingMessage {
        id: "1".into(),
        chat_jid: chat_jid.into(),
        sender_name: "Alice".into(),
        sender_jid: "tg:99:user:812".into(),
        content: "hello".into(),
        timestamp: "2026-09-08T00:00:00Z".into(),
        is_from_me: false,
        chat_type: senclaw::types::ChatType::Private,
        mentions_bot_username: None,
        bot_token: Some("bot-A".into()),
        native_msg_id: None,
        attachments: Vec::new(),
    };
    senclaw::gateway::pairing::request(db, &channel, &msg)
        .unwrap()
        .pairing
}

async fn serve() -> (String, Arc<Db>) {
    let (state, db) = temp_state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Panics here would mean the routes conflict — `/api/pairings/approve-code`
    // against `/api/pairings/:id/approve`.
    let router = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), db)
}

#[tokio::test]
async fn listing_answers_with_no_query_params() {
    let (base, db) = serve().await;
    let ch = seed(&db);
    let p = request_for(&db, ch, "tg:99:user:812");

    // No params at all: a required query field would 400 here, which is how the
    // watches endpoint silently never loaded.
    let resp = reqwest::get(format!("{base}/api/pairings")).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    let rows = v["pairings"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["code"], p.code);
    // The approver has to be shown a person, not a bare chat id.
    assert_eq!(rows[0]["senderName"], "Alice");
    assert_eq!(rows[0]["chatType"], "user");
    assert_eq!(rows[0]["channelName"], "TG");
    assert_eq!(rows[0]["status"], "pending");
}

#[tokio::test]
async fn the_all_param_is_spelled_the_way_clients_send_it() {
    let (base, db) = serve().await;
    let ch = seed(&db);
    let p = request_for(&db, ch, "tg:99:user:812");
    senclaw::gateway::pairing::reject(&db, p.id).unwrap();

    let pending: serde_json::Value = reqwest::get(format!("{base}/api/pairings"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pending["pairings"].as_array().unwrap().len(), 0);

    let resp = reqwest::get(format!("{base}/api/pairings?all=true"))
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "?all=true must not be rejected");
    let all: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(all["pairings"][0]["status"], "rejected");
}

#[tokio::test]
async fn approving_over_http_creates_the_binding() {
    let (base, db) = serve().await;
    let ch = seed(&db);
    let p = request_for(&db, ch, "tg:99:user:812");
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/api/pairings/{}/approve", p.id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["success"], true);
    assert_eq!(v["agentFolder"], "main");
    assert_eq!(v["filledPending"], true);

    assert!(db.get_binding_by_jid("tg:99:user:812").unwrap().is_some());
    assert_eq!(db.get_pending_bindings_for_channel(ch).unwrap().len(), 0);
}

#[tokio::test]
async fn approve_by_code_route_does_not_collide_with_the_id_route() {
    let (base, db) = serve().await;
    let ch = seed(&db);
    let p = request_for(&db, ch, "tg:99:user:812");

    // `approve-code` sits where `:id` would match. If axum resolved it as an id
    // this would 400 on the path parse instead of approving.
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/pairings/approve-code"))
        .json(&serde_json::json!({ "code": p.code.to_lowercase() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["chatJid"], "tg:99:user:812");
}

#[tokio::test]
async fn a_second_approval_is_refused_with_a_readable_reason() {
    let (base, db) = serve().await;
    let ch = seed(&db);
    let p = request_for(&db, ch, "tg:99:user:812");
    let client = reqwest::Client::new();
    let url = format!("{base}/api/pairings/{}/approve", p.id);

    assert_eq!(client.post(&url).send().await.unwrap().status().as_u16(), 200);

    let resp = client.post(&url).send().await.unwrap();
    // Reporting success for an update that matched nothing would tell two
    // approvers they each let this chat in.
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("đã được xử lý"), "{body}");
}

#[tokio::test]
async fn an_unknown_code_is_refused_not_silently_accepted() {
    let (base, db) = serve().await;
    seed(&db);
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/pairings/approve-code"))
        .json(&serde_json::json!({ "code": "ZZZZZZZZ" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn rejecting_over_http_leaves_the_chat_unbound() {
    let (base, db) = serve().await;
    let ch = seed(&db);
    let p = request_for(&db, ch, "tg:99:user:812");

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/pairings/{}/reject", p.id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(db.get_binding_by_jid("tg:99:user:812").unwrap().is_none());
    // The waiting binding is untouched — a rejection must not consume it.
    assert_eq!(db.get_pending_bindings_for_channel(ch).unwrap().len(), 1);
}

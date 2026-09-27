//! `/api/chats/:jid/checkpoints*` over real HTTP, against a real shadow repo.
//!
//! Route registration, the `:jid` segment with colons, JSON shapes and status
//! codes are asserted against an actual axum server — the class of bug that
//! `cargo check` and unit tests cannot see (see `telegram_pairing_api.rs`).

use std::sync::Arc;

use senclaw::checkpoints::CheckpointService;
use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

struct Harness {
    base: String,
    svc: Arc<CheckpointService>,
    work: std::path::PathBuf,
}

async fn serve() -> Harness {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("checkpoints-api-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    cfg.paths.db_path = dir.join("test.db");
    cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
    let db = Arc::new(Db::open(&cfg).unwrap());
    let svc = Arc::new(CheckpointService::new(Arc::clone(&db), &dir.join("home")));

    // A "project": a git repo (what makes a working dir eligible).
    let work = dir.join("work");
    std::fs::create_dir_all(work.join(".git")).unwrap();
    std::fs::write(work.join("a.txt"), "one\n").unwrap();

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
        checkpoints: Some(Arc::clone(&svc)),
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
    Harness {
        base: format!("http://{addr}"),
        svc,
        work,
    }
}

const JID: &str = "web:code:1";

fn url(base: &str, tail: &str) -> String {
    format!("{base}/api/chats/{}/checkpoints{tail}", urlencoding::encode(JID))
}

/// Simulate the tool loop: a Read (baseline), then an Edit that changed a file.
async fn seed_two_steps(h: &Harness) -> i64 {
    let work = h.work.to_string_lossy().to_string();
    // Read → initializes the shadow repo with the pre-edit baseline.
    let none = h
        .svc
        .on_tool_event(JID, Some(&work), "", "Read", &serde_json::json!({}), true, "Read a.txt")
        .await
        .unwrap();
    assert!(none.is_none(), "a read never records a checkpoint");
    // Edit → the file changes → one checkpoint.
    std::fs::write(h.work.join("a.txt"), "two\n").unwrap();
    let cp = h
        .svc
        .on_tool_event(JID, Some(&work), "", "Edit", &serde_json::json!({"path": "a.txt"}), true, "Edited a.txt (1 replacement)\n--- a\n+++ b")
        .await
        .unwrap()
        .expect("an edit records a checkpoint");
    assert_eq!(cp.files_changed, 1);
    cp.id
}

#[tokio::test]
async fn list_diff_restore_round_trip_over_http() {
    if !git_available() {
        return;
    }
    let h = serve().await;
    let id = seed_two_steps(&h).await;

    // List: newest first, camelCase keys, enabled by default.
    let v: serde_json::Value = reqwest::get(url(&h.base, "")).await.unwrap().json().await.unwrap();
    assert_eq!(v["enabled"], true);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "the baseline is a commit, not a row");
    assert_eq!(items[0]["id"], id);
    assert_eq!(items[0]["toolName"], "Edit");
    assert_eq!(items[0]["filesChanged"], 1);
    assert!(items[0]["parentSha"].is_string());

    // Diff against the parent (baseline).
    let d: serde_json::Value = reqwest::get(url(&h.base, &format!("/{id}/diff")))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(d["files"][0]["path"], "a.txt");
    assert_eq!(d["files"][0]["status"], "M");
    assert!(d["diff"].as_str().unwrap().contains("-one"));
    assert!(d["diff"].as_str().unwrap().contains("+two"));
    assert_eq!(d["truncated"], false);

    // Restore the whole tree to that checkpoint after a further manual edit.
    std::fs::write(h.work.join("a.txt"), "three\n").unwrap();
    let client = reqwest::Client::new();
    let r = client
        .post(url(&h.base, &format!("/{id}/restore")))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(std::fs::read_to_string(h.work.join("a.txt")).unwrap(), "two\n");
    // The restore is itself a checkpoint, so it can be undone.
    assert_eq!(body["checkpoint"]["toolName"], "restore");

    // The hand edit ("three") that no tool checkpointed was snapshotted
    // before the restore — a restore never destroys unrecorded state.
    let v: serde_json::Value = reqwest::get(url(&h.base, "")).await.unwrap().json().await.unwrap();
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["toolName"], "restore");
    assert_eq!(items[1]["toolName"], "snapshot");
    let snap_id = items[1]["id"].as_i64().unwrap();
    let r = client
        .post(url(&h.base, &format!("/{snap_id}/restore")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(std::fs::read_to_string(h.work.join("a.txt")).unwrap(), "three\n");
}

#[tokio::test]
async fn wrong_chat_and_unknown_ids_are_400() {
    if !git_available() {
        return;
    }
    let h = serve().await;
    let id = seed_two_steps(&h).await;
    let other = format!("{}/api/chats/{}/checkpoints/{id}/diff", h.base, urlencoding::encode("web:other"));
    let r = reqwest::get(other).await.unwrap();
    assert_eq!(r.status().as_u16(), 400, "a checkpoint is scoped to its chat");
    let r = reqwest::get(url(&h.base, "/99999/diff")).await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
}

#[tokio::test]
async fn settings_toggle_stops_recording() {
    if !git_available() {
        return;
    }
    let h = serve().await;
    let client = reqwest::Client::new();
    let r = client
        .put(url(&h.base, "/settings"))
        .json(&serde_json::json!({"enabled": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let work = h.work.to_string_lossy().to_string();
    std::fs::write(h.work.join("a.txt"), "two\n").unwrap();
    let cp = h
        .svc
        .on_tool_event(JID, Some(&work), "", "Edit", &serde_json::json!({}), true, "edit")
        .await
        .unwrap();
    assert!(cp.is_none(), "disabled chats record nothing");
    let v: serde_json::Value = reqwest::get(url(&h.base, "")).await.unwrap().json().await.unwrap();
    assert_eq!(v["enabled"], false);
}

#[tokio::test]
async fn a_non_git_folder_is_not_checkpointed_unless_it_is_a_code_session() {
    if !git_available() {
        return;
    }
    let h = serve().await;
    let plain = h.work.parent().unwrap().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("f.txt"), "x").unwrap();
    let dir = plain.to_string_lossy().to_string();
    let none = h
        .svc
        .on_tool_event("web:plain", Some(&dir), "", "Edit", &serde_json::json!({}), true, "edit")
        .await
        .unwrap();
    assert!(none.is_none(), "a plain folder in an ordinary chat is never crawled");
    // As a code session the same folder is eligible: a Read takes the
    // baseline, the next Edit records the change.
    h.svc
        .on_tool_event("web:plain", Some(&dir), "code", "Read", &serde_json::json!({}), true, "read")
        .await
        .unwrap();
    std::fs::write(plain.join("f.txt"), "y").unwrap();
    let some = h
        .svc
        .on_tool_event("web:plain", Some(&dir), "code", "Edit", &serde_json::json!({}), true, "edit")
        .await
        .unwrap();
    assert!(some.is_some(), "group_type=code opts a plain folder in");
}

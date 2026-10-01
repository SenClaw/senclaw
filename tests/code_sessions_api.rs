//! `/api/code/*` over real HTTP — the contract the mobile app has been calling
//! into a 404 since the old code engine was removed. Shapes are asserted
//! against `channel_app/lib/models/code_models.dart` key by key.

use std::sync::Arc;

use senclaw::checkpoints::CheckpointService;
use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::group_manager::GroupManager;
use senclaw::gateway::ui_server::{build_router, UiState};

struct Harness {
    base: String,
    dir: std::path::PathBuf,
}

async fn serve() -> Harness {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("code-sessions-api-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    cfg.paths.db_path = dir.join("test.db");
    cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
    cfg.paths.global_config_path = dir.join("config.json");
    cfg.paths.profiles_dir = dir.join("profiles");
    let db = Arc::new(Db::open(&cfg).unwrap());
    let svc = Arc::new(CheckpointService::new(Arc::clone(&db), &dir.join("home")));
    let state = Arc::new(UiState {
        config: Arc::new(cfg),
        db: Some(Arc::clone(&db)),
        group_manager: Some(Arc::new(GroupManager::new())),
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
        checkpoints: Some(svc),
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
        dir,
    }
}

async fn create(h: &Harness, name: &str, init_git: bool) -> serde_json::Value {
    let ws = h.dir.join("ws").join(name);
    let r = reqwest::Client::new()
        .post(format!("{}/api/code/sessions", h.base))
        .json(&serde_json::json!({
            "name": name,
            "workspace": ws.to_string_lossy(),
            "init_git": init_git,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "{}", r.text().await.unwrap());
    r.json().await.unwrap()
}

#[tokio::test]
async fn create_list_archive_with_mobile_shapes() {
    let h = serve().await;
    let s = create(&h, "proj-a", false).await;
    assert!(s["id"].as_str().unwrap().starts_with("code:"));
    assert_eq!(s["name"], "proj-a");
    assert_eq!(s["status"], "active");
    assert_eq!(s["git_enabled"], false);
    assert!(s["created_at"].as_i64().unwrap() > 0, "epoch millis, not a string");
    assert!(h.dir.join("ws/proj-a").is_dir(), "workspace is created");

    let list: serde_json::Value = reqwest::get(format!("{}/api/code/sessions", h.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);

    let id = s["id"].as_str().unwrap();
    let r = reqwest::Client::new()
        .delete(format!("{}/api/code/sessions/{}", h.base, urlencoding::encode(id)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let active: serde_json::Value = reqwest::get(format!("{}/api/code/sessions", h.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(active["sessions"].as_array().unwrap().len(), 0, "archived leaves the active list");
    let all: serde_json::Value = reqwest::get(format!("{}/api/code/sessions?status=all", h.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all["sessions"][0]["status"], "archived");
}

#[tokio::test]
async fn files_tree_and_content_stay_inside_the_workspace() {
    let h = serve().await;
    let s = create(&h, "proj-b", true).await;
    assert_eq!(s["git_enabled"], true, "init_git creates the project's own repo");
    let id = s["id"].as_str().unwrap().to_string();
    let ws = h.dir.join("ws/proj-b");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::create_dir_all(ws.join("node_modules/x")).unwrap();

    let enc = urlencoding::encode(&id).to_string();
    let t: serde_json::Value = reqwest::get(format!("{}/api/code/sessions/{enc}/files", h.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = t["tree"].as_array().unwrap().iter().map(|n| n["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"src"));
    assert!(!names.contains(&"node_modules"));
    assert!(!names.contains(&".git"));
    let src = t["tree"].as_array().unwrap().iter().find(|n| n["name"] == "src").unwrap();
    assert_eq!(src["type"], "dir");
    assert_eq!(src["children"][0]["path"], "src/main.rs");
    assert_eq!(src["children"][0]["type"], "file");

    let c: serde_json::Value = reqwest::get(format!(
        "{}/api/code/sessions/{enc}/file-content?path=src%2Fmain.rs",
        h.base
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(c["content"], "fn main() {}\n");

    let r = reqwest::get(format!(
        "{}/api/code/sessions/{enc}/file-content?path=..%2F..%2Ftest.db",
        h.base
    ))
    .await
    .unwrap();
    assert_eq!(r.status().as_u16(), 400, "escaping the workspace is refused");
}

#[tokio::test]
async fn git_log_is_the_checkpoint_list_and_chat_needs_an_agent() {
    let h = serve().await;
    let s = create(&h, "proj-c", false).await;
    let id = s["id"].as_str().unwrap().to_string();
    let enc = urlencoding::encode(&id).to_string();

    let log: serde_json::Value = reqwest::get(format!("{}/api/code/sessions/{enc}/git-log", h.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(log["log"].as_array().unwrap().len(), 0);

    let groups: serde_json::Value = reqwest::get(format!("{}/api/code/projects/{enc}/groups", h.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(groups["groups"][0]["id"], id, "a session is its own single group");

    let msgs: serde_json::Value = reqwest::get(format!("{}/api/code/groups/{enc}/messages", h.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(msgs["messages"].as_array().unwrap().len(), 0);

    // No agent runtime behind this test state → the route exists and answers
    // 503 (service unavailable), not 404 (which is what the mobile app used
    // to get).
    let r = reqwest::Client::new()
        .post(format!("{}/api/code/sessions/{enc}/chat", h.base))
        .json(&serde_json::json!({"prompt": "hi", "group_id": id}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 503);

    let r = reqwest::Client::new()
        .post(format!("{}/api/code/sessions/{enc}/rollback", h.base))
        .json(&serde_json::json!({"steps": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400, "nothing to roll back to yet");
}

#[tokio::test]
async fn fs_ls_lists_directories_only() {
    let h = serve().await;
    let root = h.dir.join("pick");
    std::fs::create_dir_all(root.join("b-dir")).unwrap();
    std::fs::create_dir_all(root.join("a-dir")).unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    std::fs::write(root.join("file.txt"), "x").unwrap();
    let v: serde_json::Value = reqwest::get(format!(
        "{}/api/fs/ls?path={}",
        h.base,
        urlencoding::encode(&root.to_string_lossy())
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let names: Vec<&str> = v["dirs"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["a-dir", "b-dir"]);
    assert_eq!(v["current"].as_str().unwrap(), root.to_string_lossy());
    assert!(v["parent"].is_string());
}

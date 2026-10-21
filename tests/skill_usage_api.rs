//! The skill usage ledger end to end: a load and the events that end its turn
//! go in, rows and the summary come out over real HTTP.
//!
//! `skills::usage::start` installs a process-global sender, so this binary
//! holds exactly one test that starts it.

use std::sync::Arc;
use std::time::Duration;

use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};
use senclaw::skills::metadata::parse_skill_metadata;
use senclaw::skills::usage::{note_load, record, Load};
use senclaw::skills::Skill;
use senclaw::zen_core::{EngineEvent, MessageCompleteData, ToolExecutionErrorData};

async fn serve() -> (String, Arc<Db>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::from_env();
    cfg.paths.db_path = dir.path().join("test.db");
    cfg.paths.cognitive_db_path = dir.path().join("test_cognitive.db");
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
    (format!("http://{addr}"), db, dir)
}

async fn get(base: &str, tail: &str) -> (u16, serde_json::Value) {
    let r = reqwest::get(format!("{base}{tail}")).await.unwrap();
    let status = r.status().as_u16();
    (
        status,
        r.json::<serde_json::Value>()
            .await
            .unwrap_or(serde_json::Value::Null),
    )
}

fn skill_in(dir: &std::path::Path, name: &str) -> Skill {
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    let md = format!("---\nname: {name}\ndescription: d\n---\n\nSay hi.\n");
    std::fs::write(skill_dir.join("SKILL.md"), &md).unwrap();
    Skill {
        metadata: parse_skill_metadata(&md, name, ""),
        content: "Say hi.".into(),
        file_path: skill_dir.join("SKILL.md").to_string_lossy().to_string(),
        base_dir: skill_dir.to_string_lossy().to_string(),
        locate: "clawhub-managed".into(),
    }
}

fn answered() -> EngineEvent {
    EngineEvent::MessageComplete(MessageCompleteData {
        agent_id: "main".into(),
        reasoning: String::new(),
        content: "done".into(),
        has_tool_calls: false,
        tool_calls: None,
        output_tokens: 0,
    })
}

fn tool_failed(tool: &str) -> EngineEvent {
    EngineEvent::ToolExecutionError(ToolExecutionErrorData {
        agent_id: "main".into(),
        tool_name: tool.into(),
        title: tool.into(),
        description: String::new(),
        content: "boom".into(),
        args_shape: Default::default(),
    })
}

async fn until<F: FnMut() -> bool>(mut done: F) {
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("ledger never caught up");
}

#[tokio::test]
async fn a_load_is_closed_by_the_turn_and_counted_in_the_summary() {
    let (base, db, dir) = serve().await;
    senclaw::skills::usage::start(Arc::clone(&db));
    let skill = skill_in(dir.path(), "demo");

    // Turn 1: model loads the skill, a tool fails, the turn answers.
    note_load("web:a", &skill, Load::Tool);
    record("web:a", &tool_failed("Bash"));
    record("web:a", &answered());
    // Turn 2: forced load, then the session errors out of a tool loop.
    note_load("web:a", &skill, Load::Forced);
    let looped = senclaw::zen_core::SessionErrorData {
        error_type: "tool_error_loop".into(),
        error: senclaw::zen_core::SessionErrorDetail {
            code: "tool_error_loop".into(),
            message: "stuck".into(),
            details: None,
        },
    };
    record("web:a", &EngineEvent::SessionError(looped));
    // Another chat's events change nothing here.
    record("web:b", &answered());

    let probe = Arc::clone(&db);
    until(move || {
        probe
            .list_skill_usage(None, 10)
            .map(|r| r.len() == 2 && r.iter().all(|x| x.outcome != "open"))
            .unwrap_or(false)
    })
    .await;

    let (status, body) = get(&base, "/api/skill-usage?skill=demo").await;
    assert_eq!(status, 200, "{body}");
    let loads = body["loads"].as_array().unwrap();
    assert_eq!(loads.len(), 2);
    let newest = &loads[0];
    assert_eq!(
        (
            newest["how"].as_str(),
            newest["outcome"].as_str(),
            newest["loopStopped"].as_bool()
        ),
        (Some("forced"), Some("errored"), Some(true))
    );
    let first = &loads[1];
    assert_eq!(
        (
            first["how"].as_str(),
            first["outcome"].as_str(),
            first["toolFailures"].as_i64()
        ),
        (Some("tool"), Some("completed"), Some(1))
    );
    assert_eq!(first["firstFailedTool"], "Bash");
    assert!(first["contentHash"].is_string());
    assert!(
        first.get("prompt").is_none() && first.get("args").is_none(),
        "no content is kept"
    );

    let (status, sum) = get(&base, "/api/skill-usage/summary?days=7").await;
    assert_eq!(status, 200, "{sum}");
    assert_eq!(sum["days"], 7);
    let demo = &sum["skills"][0];
    assert_eq!(
        (
            demo["key"].as_str(),
            demo["loads"].as_i64(),
            demo["errored"].as_i64()
        ),
        (Some("demo"), Some(2), Some(1))
    );
    assert_eq!(demo["fallbackRate"], 0.5);
    assert_eq!(demo["toolFailureRate"], 0.5);
    assert_eq!(
        demo["versions"].as_array().unwrap().len(),
        1,
        "one content version"
    );
}

#![allow(unused_imports)]

use crate::types::GroupBinding;
use std::collections::HashSet;
use std::sync::Arc;

use super::workspace::WorkspaceStateFile;
use super::{AgentPool, PermissionsConfig, ZenCoreApi};

fn fake_binding(jid: &str) -> GroupBinding {
    GroupBinding {
        jid: jid.into(),
        folder: "test".into(),
        name: "Test".into(),
        channel: "web".into(),
        group_type: "chat".into(),
        requires_trigger: false,
        allowed_tools: None,
        allowed_paths: None,
        allowed_work_dirs: None,
        bot_token: None,
        max_messages: None,
        llm_config_id: None,
        last_active: None,
        added_at: "2026-01-01T00:00:00Z".into(),
    }
}

#[test]
fn agent_pool_send_reply_no_callback_does_not_panic() {
    let pool = AgentPool::new(Arc::new(ZenCoreApi::new(None)));
    // Default permissions config is all-false.
    let cfg = pool.get_permissions_config();
    assert!(!cfg.skip_main_agent_permissions);
    assert!(!cfg.skip_all_agents_permissions);
    // notify_activity on unknown JID is a no-op.
    pool.notify_activity("nobody:0");
}

#[test]
fn permissions_config_round_trips() {
    let pool = AgentPool::new(Arc::new(ZenCoreApi::new(None)));
    pool.set_permissions_config(PermissionsConfig {
        skip_main_agent_permissions: true,
        skip_all_agents_permissions: false,
    });
    let cfg = pool.get_permissions_config();
    assert!(cfg.skip_main_agent_permissions);
    assert!(!cfg.skip_all_agents_permissions);
    // Virtual/persona agents only skip permissions under the ALL-agents flag —
    // the main-agent flag alone must not leak to them.
    assert!(!pool.get_skip_perms_for_virtual());

    pool.set_permissions_config(PermissionsConfig {
        skip_main_agent_permissions: false,
        skip_all_agents_permissions: true,
    });
    assert!(pool.get_skip_perms_for_virtual());
}

/// Found by running a real session: the saved "skip approval" choice was
/// loaded into the pool at boot but never reached an engine created
/// afterwards, because `ensure_engine` builds its options from `Default`.
/// The switch therefore worked on a live chat and silently stopped working
/// after a restart — and a headless daemon has no UI to answer the prompt.
#[test]
fn a_new_engine_inherits_the_saved_skip_choice() {
    let api = Arc::new(ZenCoreApi::new(None));
    let pool = AgentPool::new(Arc::clone(&api) as Arc<dyn crate::agent::agent_pool::CoreApi>);

    // No agent exists yet, which is exactly the boot case: the hot-update
    // loop has nothing to update, so only the default can carry the choice.
    pool.set_permissions_config(PermissionsConfig {
        skip_main_agent_permissions: false,
        skip_all_agents_permissions: true,
    });
    assert!(
        api.default_skip_permissions_for_test(),
        "an engine created after this must skip approval"
    );

    pool.set_permissions_config(PermissionsConfig {
        skip_main_agent_permissions: false,
        skip_all_agents_permissions: false,
    });
    assert!(
        !api.default_skip_permissions_for_test(),
        "turning it back off must also reach engines created later"
    );
}

#[test]
fn thinking_default_on() {
    let pool = AgentPool::new(Arc::new(ZenCoreApi::new(None)));
    assert!(pool.get_thinking_enabled());
    pool.set_thinking_enabled(false);
    assert!(!pool.get_thinking_enabled());
}

#[test]
fn skip_perms_main_flag_applies_uniformly() {
    // Every chat is admin now: `skip_main_agent_permissions` skips prompts for
    // all agents, with no per-binding or dispatch distinction.
    let opts = PermissionsConfig {
        skip_main_agent_permissions: true,
        skip_all_agents_permissions: false,
    };
    assert!(AgentPool::compute_skip_perms(&opts));
}

#[test]
fn skip_perms_none_set_requires_prompts() {
    let opts = PermissionsConfig {
        skip_main_agent_permissions: false,
        skip_all_agents_permissions: false,
    };
    assert!(!AgentPool::compute_skip_perms(&opts));
}

#[test]
fn skip_perms_skip_all_overrides_everything() {
    let opts = PermissionsConfig {
        skip_main_agent_permissions: false,
        skip_all_agents_permissions: true,
    };
    assert!(AgentPool::compute_skip_perms(&opts));
}

#[test]
fn dispatch_executing_mark_clear() {
    let pool = AgentPool::new(Arc::new(ZenCoreApi::new(None)));
    pool.mark_dispatch_executing("g:1");
    assert!(pool
        .state
        .lock()
        .unwrap()
        .dispatch_executing
        .contains("g:1"));
    pool.clear_dispatch_executing("g:1");
    assert!(!pool
        .state
        .lock()
        .unwrap()
        .dispatch_executing
        .contains("g:1"));
}

#[test]
fn dispatch_task_map_round_trip() {
    let pool = AgentPool::new(Arc::new(ZenCoreApi::new(None)));
    pool.set_current_dispatch_task_id("g:1", "task-42");
    let s = pool.state.lock().unwrap();
    assert_eq!(
        s.dispatch_task_map.get("g:1").map(String::as_str),
        Some("task-42")
    );
}

#[test]
fn notify_dispatch_skips_when_no_pending_reply() {
    let pool = AgentPool::new(Arc::new(ZenCoreApi::new(None)));
    // No content recorded → silent no-op (no panic).
    pool.notify_dispatch_if_pending("g:1", Some("task-1"));
}

#[test]
fn workspace_state_file_path_format() {
    let pool = AgentPool::new(Arc::new(ZenCoreApi::new(None)));
    let tmp = std::env::temp_dir().join(format!("senclaw-test-{}", std::process::id()));
    pool.set_senclaw_home(tmp.clone());
    let p = pool.workspace_state_file("main");
    assert_eq!(p, tmp.join("workspace-state-main.json"));
}

#[test]
fn init_workspace_state_writes_default() {
    let tmp = tempfile::tempdir().unwrap();
    let state_file = tmp.path().join("workspace-state-foo.json");
    let default_dir = tmp.path().join("foo-workspace");
    AgentPool::init_workspace_state(&state_file, &default_dir);
    let raw = std::fs::read_to_string(&state_file).unwrap();
    let parsed: WorkspaceStateFile = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed.current_dir, default_dir.to_string_lossy());
    assert!(!parsed.updated_at.is_empty());
}

#[test]
fn init_workspace_state_skips_when_file_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let state_file = tmp.path().join("ws.json");
    std::fs::write(&state_file, r#"{"currentDir":"/custom","updatedAt":""}"#).unwrap();
    AgentPool::init_workspace_state(&state_file, &tmp.path().join("default"));
    let raw = std::fs::read_to_string(&state_file).unwrap();
    assert!(raw.contains("/custom"));
}

#[test]
fn cached_todos_empty_by_default() {
    let pool = AgentPool::new(Arc::new(ZenCoreApi::new(None)));
    assert!(pool.get_all_cached_todos().is_empty());
}

// ── Image attachment routing ────────────────────────────────────────────────

/// A `CoreApi` that records which dispatch path the pool chose and with what.
#[derive(Default)]
struct RecordingCore {
    text_calls: std::sync::Mutex<Vec<String>>,
    image_calls: std::sync::Mutex<Vec<(String, usize)>>,
}

impl super::traits::CoreApi for RecordingCore {
    fn process_user_input(&self, _jid: &str, prompt: &str) -> anyhow::Result<()> {
        self.text_calls.lock().unwrap().push(prompt.to_string());
        Ok(())
    }

    fn process_user_input_with_images(
        &self,
        _jid: &str,
        prompt: &str,
        images: Vec<crate::zen_core::ImageSource>,
    ) -> anyhow::Result<()> {
        self.image_calls
            .lock()
            .unwrap()
            .push((prompt.to_string(), images.len()));
        Ok(())
    }
}

fn one_image() -> Vec<crate::zen_core::ImageSource> {
    vec![crate::zen_core::ImageSource {
        source_type: "base64".into(),
        media_type: "image/png".into(),
        data: "QUJD".into(),
    }]
}

/// Build a pool whose config points at a throwaway `config.json` holding a
/// single LLM config with the given model name.
fn pool_with_model(model: &str) -> (Arc<AgentPool>, Arc<RecordingCore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    crate::gateway::group_manager::save_llm_config(
        &config_path,
        &crate::gateway::group_manager::LlmConfig {
            id: "llm_1".into(),
            label: "Test".into(),
            provider: "openai".into(),
            base_url: "https://example.invalid/v1".into(),
            api_key: "k".into(),
            model_name: model.into(),
            adapt: "openai".into(),
            max_tokens: 4096,
            context_length: 128_000,
            vision: None,
            ..Default::default()
        },
    )
    .unwrap();
    crate::gateway::group_manager::set_active_llm_config(&config_path, Some("llm_1")).unwrap();

    let core = Arc::new(RecordingCore::default());
    let pool = AgentPool::new(core.clone() as Arc<dyn super::traits::CoreApi>);
    let mut cfg = crate::config::Config::from_env();
    cfg.paths.global_config_path = config_path;
    // Point OCR at an empty dir so the no-vision path takes its "unavailable"
    // branch deterministically instead of picking up a real installed model.
    cfg.paths.ocr_models_dir = dir.path().join("ocr-models");
    // Keep attachment writes inside the tempdir, never the real ~/.senclaw.
    cfg.paths.uploads_dir = dir.path().join("uploads");
    pool.set_config(Arc::new(cfg));
    (pool, core, dir)
}

#[tokio::test]
async fn vision_model_receives_the_image_blocks() {
    // The regression: the pool used to flatten the built input to its text
    // blocks, so an attached image never reached the model at all.
    let (pool, core, _dir) = pool_with_model("gpt-4o");
    pool.dispatch_user_input(
        "web:1",
        &fake_binding("web:1"),
        "Ảnh này là gì?",
        one_image(),
    )
    .await
    .unwrap();

    let images = core.image_calls.lock().unwrap();
    assert_eq!(images.len(), 1, "should take the vision path");
    assert_eq!(images[0].0, "Ảnh này là gì?", "prompt travels unchanged");
    assert_eq!(images[0].1, 1, "the image block must survive");
    assert!(core.text_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn text_only_model_gets_an_ocr_notice_instead_of_image_blocks() {
    let (pool, core, _dir) = pool_with_model("deepseek-chat");
    pool.dispatch_user_input(
        "web:1",
        &fake_binding("web:1"),
        "Ảnh này là gì?",
        one_image(),
    )
    .await
    .unwrap();

    assert!(
        core.image_calls.lock().unwrap().is_empty(),
        "image blocks must never reach a text-only endpoint"
    );
    let texts = core.text_calls.lock().unwrap();
    assert_eq!(texts.len(), 1);
    assert!(texts[0].starts_with("Ảnh này là gì?"));
    assert!(texts[0].contains("[attached-images: 1]"));
    // No OCR model installed here, so the model must be told not to guess.
    assert!(texts[0].contains("Do NOT guess"));
}

#[tokio::test]
async fn imageless_turns_keep_the_plain_text_path() {
    let (pool, core, _dir) = pool_with_model("gpt-4o");
    pool.dispatch_user_input("web:1", &fake_binding("web:1"), "chào", Vec::new())
        .await
        .unwrap();

    assert!(core.image_calls.lock().unwrap().is_empty());
    assert_eq!(core.text_calls.lock().unwrap().as_slice(), ["chào"]);
}

#[tokio::test]
async fn group_model_override_decides_the_route() {
    // Active config is vision-capable; the group pins a text-only one. The
    // turn runs on the group's model, so the routing must follow it.
    let (pool, core, dir) = pool_with_model("gpt-4o");
    let config_path = dir.path().join("config.json");
    crate::gateway::group_manager::save_llm_config(
        &config_path,
        &crate::gateway::group_manager::LlmConfig {
            id: "llm_text".into(),
            label: "Text".into(),
            provider: "openai".into(),
            base_url: "https://example.invalid/v1".into(),
            api_key: "k".into(),
            model_name: "deepseek-chat".into(),
            adapt: "openai".into(),
            max_tokens: 4096,
            context_length: 128_000,
            vision: None,
            ..Default::default()
        },
    )
    .unwrap();

    let mut group = fake_binding("web:1");
    group.llm_config_id = Some("llm_text".into());
    pool.dispatch_user_input("web:1", &group, "Ảnh này là gì?", one_image())
        .await
        .unwrap();

    assert!(core.image_calls.lock().unwrap().is_empty());
    assert_eq!(core.text_calls.lock().unwrap().len(), 1);
}

// ── Document attachments ────────────────────────────────────────────────────

fn doc(name: &str, mime: &str, body: &str) -> crate::types::MessageAttachment {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    crate::types::MessageAttachment {
        data_url: format!("data:{mime};base64,{}", STANDARD.encode(body)),
        mime_type: mime.into(),
        name: Some(name.into()),
    }
}

fn image_att() -> crate::types::MessageAttachment {
    crate::types::MessageAttachment {
        data_url: "data:image/png;base64,QUJD".into(),
        mime_type: "image/png".into(),
        name: None,
    }
}

/// Drive the attachment pipeline the way a real turn does: prepare, then
/// dispatch. Skips the surrounding process-and-wait machinery, which needs a
/// daemon-initialised memory manager.
async fn run_turn(
    pool: &Arc<AgentPool>,
    group: &GroupBinding,
    prompt: &str,
    attachments: &[crate::types::MessageAttachment],
) {
    let (text, images) = pool
        .prepare_turn_input(&group.jid, prompt, attachments)
        .await;
    pool.dispatch_user_input(&group.jid, group, &text, images)
        .await
        .unwrap();
}

#[tokio::test]
async fn document_text_reaches_the_prompt_and_the_file_lands_on_disk() {
    let (pool, core, dir) = pool_with_model("gpt-4o");
    run_turn(
        &pool,
        &fake_binding("web:1"),
        "Tóm tắt file này",
        &[doc("ghi-chu.txt", "text/plain", "nội dung quan trọng")],
    )
    .await;

    let texts = core.text_calls.lock().unwrap();
    let prompt = texts.first().expect("core should have received a turn");
    assert!(prompt.contains("Tóm tắt file này"));
    assert!(prompt.contains("[attached-files: 1]"));
    assert!(prompt.contains("nội dung quan trọng"));
    assert!(prompt.contains("saved at: "));
    // No image blocks — a document is not an image even on a vision model.
    assert!(core.image_calls.lock().unwrap().is_empty());

    let saved = std::fs::read_dir(dir.path().join("uploads").join("web_1"))
        .unwrap()
        .filter_map(Result::ok)
        .count();
    assert_eq!(
        saved, 1,
        "the attachment should be written under the jid dir"
    );
}

#[tokio::test]
async fn an_unreadable_document_still_reaches_the_model_as_a_path() {
    // The point of saving before extracting: a format we can't parse is still
    // something the agent can open with its own tools.
    let (pool, core, _dir) = pool_with_model("gpt-4o");
    run_turn(
        &pool,
        &fake_binding("web:1"),
        "File này nói gì?",
        &[doc(
            "bao-cao.pdf",
            "application/pdf",
            "%PDF-1.4 binary junk",
        )],
    )
    .await;

    let texts = core.text_calls.lock().unwrap();
    let prompt = texts.first().unwrap();
    assert!(prompt.contains("could not extract text"));
    assert!(prompt.contains("bao-cao.pdf"));
    assert!(prompt.contains("saved at: "));
    assert!(prompt.contains("Never invent contents"));
}

#[tokio::test]
async fn images_and_documents_on_one_turn_take_their_own_routes() {
    let (pool, core, _dir) = pool_with_model("gpt-4o");
    run_turn(
        &pool,
        &fake_binding("web:1"),
        "Xem hai thứ này",
        &[image_att(), doc("a.md", "text/markdown", "# tiêu đề")],
    )
    .await;

    // The image goes as a block, the document as text — in the same turn.
    let images = core.image_calls.lock().unwrap();
    assert_eq!(images.len(), 1, "vision model should get the image block");
    assert_eq!(images[0].1, 1);
    assert!(images[0].0.contains("[attached-files: 1]"));
    assert!(images[0].0.contains("# tiêu đề"));
    assert!(core.text_calls.lock().unwrap().is_empty());
}

#[test]
fn a_saved_permission_choice_reaches_the_pool() {
    // The seam that was missing: `config.json` held the user's choice and the
    // Web UI's save handler was the only thing that ever pushed it into the
    // pool. A daemon that booted without anyone opening the UI ran with both
    // flags false, so every dispatch worker stopped at its first Write waiting
    // on an admin client that a headless daemon does not have.
    use crate::gateway::group_manager::{
        get_admin_permissions_config, save_admin_permissions_config, AdminPermissions,
    };

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    save_admin_permissions_config(
        &config_path,
        &AdminPermissions {
            skip_main_agent_permissions: false,
            skip_all_agents_permissions: true,
        },
    )
    .unwrap();

    let saved = get_admin_permissions_config(&config_path);
    let pool = AgentPool::new(Arc::new(ZenCoreApi::new(None)));
    assert!(
        !pool.get_skip_perms_for_virtual(),
        "a fresh pool must start locked down"
    );

    pool.set_permissions_config(PermissionsConfig {
        skip_main_agent_permissions: saved.skip_main_agent_permissions,
        skip_all_agents_permissions: saved.skip_all_agents_permissions,
    });
    assert!(
        pool.get_skip_perms_for_virtual(),
        "boot must apply the saved choice, or dispatch workers hang on approval"
    );
}

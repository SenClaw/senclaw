use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
    routing::{any, delete, get, patch, post, put},
    Router,
};
use tower_http::services::ServeDir;

use crate::config::Config;
use crate::db::Db;
use crate::mcp::manager::McpManager;
use crate::wiki::manager::WikiManager;

use super::chat::{
    chat_form_respond, chat_history, chat_permission_respond, chat_plan_respond,
    chat_question_respond, chat_states,
};
use super::code::code_run;
use super::code_artifacts::{
    create_artifact, delete_artifact, get_artifact, list_artifacts, run_artifact, update_artifact,
};
use super::config_handler::{admin_perms_get, admin_perms_set, config_handler, thinking_handler};
use super::embedding_config::{embedding_config_get, embedding_config_save};
use super::llm_config::{
    llm_config_create, llm_config_delete, llm_config_fetch_models, llm_config_list,
    llm_config_set_active, llm_config_test, llm_config_update,
};
use super::marketplace::{
    marketplace_hub_install, marketplace_mcp_status, marketplace_mcp_use_tools,
    marketplace_plugin_install, marketplace_plugin_toggle, marketplace_plugin_uninstall,
    marketplace_source_catalog, marketplace_source_disable_all, marketplace_source_enable_all,
    marketplace_source_get, marketplace_sources_add, marketplace_sources_delete,
    marketplace_sources_list, marketplace_sources_reorder, marketplace_sources_sync,
};
use super::mcp::{
    hooks_get, hooks_put, mcp_servers_connect, mcp_servers_delete, mcp_servers_disconnect,
    mcp_servers_enabled, mcp_servers_get, mcp_servers_list, mcp_servers_save, mcp_servers_test,
    mcp_servers_tools,
};
use super::open_url::open_url_handler;
use crate::local_models::rest as local_models_rest;
use crate::runtime::{proxy as runtime_proxy, rest as runtime_rest};
use super::plugins::{
    plugins_configure, plugins_disable, plugins_enable, plugins_get, plugins_install, plugins_list,
    plugins_remote_search, plugins_uninstall,
};
use super::quicknotes::quicknotes_save;
use super::skills::{
    skills_create, skills_install, skills_list, skills_readme, skills_readme_save,
    skills_remote_search, skills_toggle, skills_uninstall,
};
use super::spa::spa_fallback;
use super::space::{
    space_app_config_delete, space_app_config_get, space_app_config_list, space_app_config_set,
    space_app_env, space_app_logs_clear, space_app_logs_get, space_app_mcp_info,
    space_app_mcp_register, space_app_requirements, space_app_sandbox_get, space_app_sandbox_put,
    space_app_sqlite_query, space_app_token_get, space_app_token_mode_get,
    space_app_token_mode_put, space_app_token_rotate, space_apps_bridge, space_apps_delete,
    space_apps_install_zip, space_apps_list, space_apps_proxy, space_apps_proxy_root,
    space_apps_ready, space_apps_register, space_apps_register_local, space_apps_restart,
    space_apps_start, space_apps_static, space_apps_status, space_apps_stop, space_apps_update,
    space_apps_updates, space_events_create, space_events_delete, space_events_get,
    space_events_list, space_events_search, space_events_set_reminder, space_events_update,
    space_notes_create, space_notes_delete, space_notes_list, space_notes_search,
    space_notes_update, space_schedules_cancel, space_schedules_create, space_schedules_detail,
    space_schedules_list, space_schedules_run_now, space_schedules_update,
    space_screenshot_extract, space_screenshot_get, space_sync_apple_calendar,
    space_sync_apple_notes, space_sync_google_calendar, space_sync_google_workspace,
    space_today_summary,
};
use super::subagents::{
    subagents_create, subagents_list, subagents_readme, subagents_readme_save, subagents_toggle,
};
use super::decision::{
    decision_gate_check, decision_gate_get, decision_gate_put, decision_skills_check,
    decision_skills_get, decision_skills_put,
};
use super::control_plane::{decisions_replay, spec_mode_put, specs_list, trace_get, traces_list};
use super::types::AdminPermissionsConfig;
use super::wiki::{
    wiki_dir_delete, wiki_file_delete, wiki_history, wiki_mkdir, wiki_read, wiki_search,
    wiki_stats, wiki_tags, wiki_tree, wiki_upload, wiki_write,
};

// ===== Trait for AgentPool-dependent operations =====

/// Operations the UI server needs from AgentPool (stubbed until sema-core arrives).
#[async_trait]
pub trait UiApi: Send + Sync {
    /// Signal all agents to reload their skill registries.
    fn reload_all_skills(&self) {}
    /// Signal all agents to re-read hook config from disk.
    fn reload_all_hooks(&self) {}
    /// Push a small event to every connected admin socket.
    ///
    /// For "something you are displaying changed, re-fetch it" signals. Send
    /// the *fact* of the change, not the data: this fans out to every admin
    /// client, which is a wider audience than any single file's own tier rule
    /// assumes. Default is a no-op so bare test setups need no gateway.
    fn broadcast_event(&self, _event: serde_json::Value) {}
    /// Tell clients showing `jid` that a checkpoint was just made (a restore
    /// from the REST API records one too). Default no-op for bare test setups.
    fn broadcast_checkpoint_new(&self, _jid: &str, _cp: &crate::types::ChatCheckpoint) {}
    /// Queue a user message into `group`'s agent exactly as the WS
    /// `message:send` path does. `Err` = no agent runtime behind this state.
    fn submit_user_message(&self, _group: &crate::types::GroupBinding, _text: &str) -> Result<(), String> {
        Err("no agent runtime".into())
    }
    /// Interrupt whatever `jid`'s agent is doing.
    fn stop_agent(&self, _jid: &str) {}
    /// Pin the working directory of a chat (code sessions do this at creation).
    fn set_working_dir(&self, _jid: &str, _dir: &str) {}
    /// Get current thinking-enabled state.
    fn get_thinking_enabled(&self) -> bool {
        false
    }
    /// Set thinking-enabled state.
    fn set_thinking_enabled(&self, _enabled: bool) {}
    /// Get current admin permissions config.
    fn get_permissions_config(&self) -> AdminPermissionsConfig {
        AdminPermissionsConfig::default()
    }
    /// Set admin permissions config.
    fn set_permissions_config(&self, _cfg: AdminPermissionsConfig) {}

    /// Resolve a pending tool-permission request (mobile parity with the web
    /// WS `permission:response`). No-op by default.
    fn resolve_permission(&self, _request_id: &str, _option_key: &str) {}

    /// Resolve a pending ask-question batch. `answers` is keyed by question
    /// index → selected option index (or array for multi-select); `-1` means
    /// the "Other" free-text in `other_texts`.
    fn resolve_ask_question(
        &self,
        _request_id: &str,
        _answers: &serde_json::Value,
        _other_texts: Option<&serde_json::Value>,
    ) {
    }

    /// Resolve a pending FormUI form. `values` is keyed by field `key`;
    /// `submitted = false` means the user skipped. No-op by default.
    fn resolve_form(&self, _request_id: &str, _values: &serde_json::Value, _submitted: bool) {}

    /// Resolve a pending ExitPlanMode request. `selected` is
    /// `startEditing` | `clearContextAndStart` | (anything else = cancelled).
    fn resolve_plan_exit(&self, _group_jid: &str, _agent_id: &str, _selected: &str) {}

    /// Send one plain message into a channel chat, outside any agent turn.
    ///
    /// Exists for pairing approval: the chat that was told to wait has no other
    /// way to learn it was let in, and silence there is indistinguishable from
    /// a rejected request. No-op by default so bare test setups need no pool.
    fn send_channel_message(&self, _chat_jid: &str, _text: &str, _bot_token: Option<&str>) {}
}

// ===== Shared state =====

pub struct UiState {
    pub config: Arc<Config>,
    pub db: Option<Arc<Db>>,
    pub group_manager: Option<Arc<crate::gateway::group_manager::GroupManager>>,
    pub wiki_manager: Option<Arc<WikiManager>>,
    pub persona_registry: Option<Arc<Mutex<crate::agent::persona_registry::PersonaRegistry>>>,
    pub agent_api: Option<Arc<dyn UiApi>>,
    pub mcp_manager: Option<Arc<McpManager>>,
    pub marketplace_manager: Option<Arc<Mutex<crate::marketplace::manager::MarketplaceManager>>>,
    pub workbench_bridge: Option<Arc<crate::agent::workbench_bridge::WorkbenchBridge>>,
    /// DAG sub-agent dispatch. Backs the read-only `GET /api/dispatch`
    /// snapshot — `dispatch:update` goes to WebSocket admin clients only, so a
    /// relay client polls this instead of subscribing.
    pub dispatch_bridge: Option<Arc<crate::agent::dispatch_bridge::DispatchBridge>>,
    pub space_mcp_launcher: Option<Arc<super::space_mcp::SpaceMcpLauncher>>,
    pub workflow_service: Option<Arc<crate::workflow::WorkflowService>>,
    /// Headless agent runtime (tools + MCP + browser). Lets Space Apps run a
    /// full tool-enabled agent via the `agent.run` bridge action.
    pub virtual_worker_pool: Option<Arc<crate::agent::virtual_worker_pool::VirtualWorkerPool>>,
    /// Autonomous background work (no chat session). Backs `/api/background/*`.
    pub background_scheduler: Option<Arc<crate::background::BackgroundScheduler>>,
    /// Live per-group agent state map (`jid → "processing"/"idle"/…`), shared
    /// with the WebSocket gateway's `last_known_states`. Backs
    /// `GET /api/chat/states` so relay clients can reconcile after a drop.
    pub agent_states: Option<Arc<tokio::sync::Mutex<std::collections::HashMap<String, String>>>>,
    /// Token accounting sink for LLM calls the UI server brokers (bridge
    /// `llm.request`, internal draft completions). `None` in bare test setups.
    pub usage_recorder: Option<Arc<crate::usage::UsageRecorder>>,
    /// Shadow-git checkpoints per chat (`/api/chats/:jid/checkpoints*`).
    pub checkpoints: Option<Arc<crate::checkpoints::CheckpointService>>,
    /// Runtime manager — installed engine packages and the processes the
    /// daemon supervises for them (`/api/runtimes/*`, the legacy-namespace
    /// proxy, the local-model route). `None` in bare test setups.
    pub runtime_manager: Option<Arc<crate::runtime::RuntimeManager>>,
    pub ws_port: u16,
    pub ws_token: String,
    /// API access-token policy. `ApiAuth::disabled()` for the default
    /// loopback bind; enforcing when the daemon is exposed beyond loopback.
    /// The middleware itself is layered at the serve sites (`lib.rs`,
    /// `start_ui_server`) — the relay bridge reuses `build_router` without it
    /// because relay frames are authenticated by relay pairing instead.
    pub api_auth: Arc<super::auth::ApiAuth>,
}

/// Return the web/dist directory, falling back to cwd-based path.
fn resolve_dist_dir() -> PathBuf {
    // Desktop app bundles web/dist as a resource and points here via env;
    // `senclaw web` sets it to the downloaded release bundle.
    if let Ok(dir) = std::env::var("SENCLAW_WEB_DIST") {
        let p = PathBuf::from(dir);
        if p.exists() {
            return p;
        }
    }
    // Try relative to the current working directory next.
    let cwd_dist = PathBuf::from("web/dist");
    if cwd_dist.exists() {
        return cwd_dist;
    }
    // Dev fallback: the Web UI is its own sibling repository now
    // (`web-app/`, cloned next to this one) rather than a `web/` subdirectory
    // here — `cargo run` from this repo finds its build at `../web-app/dist`.
    let sibling_dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../web-app/dist");
    if sibling_dist.exists() {
        return sibling_dist;
    }
    cwd_dist
}

// ===== Router construction =====

pub fn build_router(state: Arc<UiState>) -> Router {
    let dist_dir = resolve_dist_dir();

    // SPA fallback for client-side routes: if ServeDir can't resolve a
    // path to a real file, hand back index.html with HTTP 200 so
    // React-Router takes over. Without this any deep-link URL like
    // /chat/cowork:abc returns a hard 404 and the bundle never loads.
    let index_path = dist_dir.join("index.html");
    let spa_index = tower_http::services::ServeFile::new(&index_path);
    let serve_dir = ServeDir::new(&dist_dir)
        .precompressed_gzip()
        .precompressed_br()
        .fallback(spa_index);

    // OS-sandbox engine (`src/sandbox`): the whole Space-App REST surface,
    // nested under /api/sandbox. Carries its own state (the engine DB); when
    // the engine cannot open, the subtree answers 503 instead of vanishing.
    let sandbox_router = match crate::sandbox::shared_db() {
        Some(db) => crate::sandbox::api::api_router(crate::sandbox::state::AppState { db }),
        None => Router::new().fallback(|| async {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "sandbox engine unavailable (data dir or DB failed to open — see daemon log)"
                })),
            )
        }),
    };

    // Token handshake for remote (non-loopback) clients. Routed on a plain
    // sub-router so the handlers see `Arc<ApiAuth>` state directly.
    // `/api/auth/mode` is deliberately *not* in `OPEN_API_PATHS`: it is the
    // gate's own switch, so an anonymous remote client must not be able to
    // read it, let alone turn it off.
    let auth_router = Router::new()
        .route("/api/auth/login", post(super::auth::auth_login))
        .route("/api/auth/status", get(super::auth::auth_status))
        .route(
            "/api/auth/mode",
            get(super::auth::auth_mode_get).put(super::auth::auth_mode_put),
        )
        .with_state(Arc::clone(&state.api_auth));

    Router::new()
        // API endpoints
        .merge(auth_router)
        .nest_service("/api/sandbox", sandbox_router)
        .route("/api/config", get(config_handler))
        // The API's own contract, generated from these routers (see openapi.rs).
        .route("/api/openapi.json", get(super::openapi::openapi_json))
        // Open a URL in the host machine's default browser (see open_url.rs).
        .route("/api/ui/open-url", post(open_url_handler))
        .route("/api/skills", get(skills_list))
        .route("/api/skills/remote-search", get(skills_remote_search))
        .route("/api/skills/create", post(skills_create))
        .route("/api/skills/install", post(skills_install))
        .route("/api/skills/:name", delete(skills_uninstall))
        .route(
            "/api/skills/:name/readme",
            get(skills_readme).put(skills_readme_save),
        )
        .route("/api/skills/:name/:action", post(skills_toggle))
        // ── Plugins API ──────────────────────────────────────────────────────
        .route("/api/plugins", get(plugins_list))
        .route("/api/plugins/remote-search", get(plugins_remote_search))
        .route("/api/plugins/install", post(plugins_install))
        .route(
            "/api/plugins/:slug",
            get(plugins_get).delete(plugins_uninstall),
        )
        .route("/api/plugins/:slug/enable", post(plugins_enable))
        .route("/api/plugins/:slug/disable", post(plugins_disable))
        .route("/api/plugins/:slug/configure", post(plugins_configure))
        // ── Marketplace API ──────────────────────────────────────────────────────
        .route(
            "/api/marketplace/sources",
            get(marketplace_sources_list).post(marketplace_sources_add),
        )
        .route(
            "/api/marketplace/sources/reorder",
            post(marketplace_sources_reorder),
        )
        .route(
            "/api/marketplace/hub/install",
            post(marketplace_hub_install),
        )
        .route(
            "/api/marketplace/sources/:id",
            get(marketplace_source_get).delete(marketplace_sources_delete),
        )
        .route(
            "/api/marketplace/sources/:id/sync",
            post(marketplace_sources_sync),
        )
        .route(
            "/api/marketplace/sources/:id/enable-all",
            post(marketplace_source_enable_all),
        )
        .route(
            "/api/marketplace/sources/:id/disable-all",
            post(marketplace_source_disable_all),
        )
        .route(
            "/api/marketplace/sources/:id/plugins/:name/toggle",
            post(marketplace_plugin_toggle),
        )
        .route(
            "/api/marketplace/sources/:id/catalog",
            get(marketplace_source_catalog),
        )
        .route(
            "/api/marketplace/sources/:id/plugins/:name/install",
            post(marketplace_plugin_install),
        )
        .route(
            "/api/marketplace/sources/:id/plugins/:name",
            delete(marketplace_plugin_uninstall),
        )
        .route(
            "/api/marketplace/sources/:id/plugins/:name/mcp/:server/use-tools",
            post(marketplace_mcp_use_tools),
        )
        .route("/api/marketplace/mcp-status", get(marketplace_mcp_status))
        // Zen Kits — bundles of personas/skills/workflows/hooks/jobs the
        // daemon installs itself, so every client behaves the same way.
        .route("/api/kits", get(super::kits::kits_list))
        .route("/api/kits/preview", post(super::kits::kits_preview))
        .route("/api/kits/install", post(super::kits::kits_install))
        .route("/api/kits/available", get(super::kits::kits_available))
        .route(
            "/api/kits/available/preview",
            post(super::kits::kits_available_preview),
        )
        .route(
            "/api/kits/available/install",
            post(super::kits::kits_available_install),
        )
        .route("/api/kits/:id", delete(super::kits::kits_uninstall))
        // ===== Zen Patterns =====
        .route(
            "/api/patterns",
            get(super::patterns::patterns_list).post(super::patterns::patterns_save),
        )
        .route("/api/patterns/run", post(super::patterns::patterns_run))
        .route("/api/patterns/import", post(super::patterns::patterns_import))
        .route("/api/patterns/catalog", get(super::patterns::catalog_list))
        .route(
            "/api/patterns/catalog/:id/install",
            post(super::patterns::catalog_install),
        )
        .route(
            "/api/patterns/sources",
            get(super::patterns::sources_list).post(super::patterns::sources_add),
        )
        .route(
            "/api/patterns/sources/:id/sync",
            post(super::patterns::sources_sync),
        )
        .route(
            "/api/patterns/sources/:id/toggle",
            post(super::patterns::sources_toggle),
        )
        .route(
            "/api/patterns/sources/:id",
            delete(super::patterns::sources_delete),
        )
        // Registered after every literal sibling above, so `/run`, `/import`
        // and `/sources` are matched as themselves and never as a pattern
        // named "run".
        .route(
            "/api/patterns/:name",
            get(super::patterns::patterns_get).delete(super::patterns::patterns_delete),
        )
        .route("/api/subagents", get(subagents_list))
        .route("/api/subagents/create", post(subagents_create))
        .route(
            "/api/subagents/:name/readme",
            get(subagents_readme).put(subagents_readme_save),
        )
        .route("/api/subagents/:name/:action", post(subagents_toggle))
        .route("/api/thinking", post(thinking_handler))
        .route(
            "/api/agent-behavior",
            get(super::agent_behavior_config::agent_behavior_get)
                .post(super::agent_behavior_config::agent_behavior_set),
        )
        // Widget catalog + default-flow settings (Plugins → Widget).
        .route("/api/widgets", get(super::widgets::widgets_list))
        .route("/api/widgets/:id", put(super::widgets::widget_toggle))
        .route(
            "/api/defaults",
            get(super::widgets::defaults_get).put(super::widgets::defaults_set),
        )
        // Enabled marketplace plugins' widget assets (widgets/ dir).
        .route(
            "/api/marketplace/plugins/:name/widget-static/*path",
            get(super::marketplace::plugin_widget_static),
        )
        .route("/api/dispatch", get(super::dispatch::dispatch_snapshot))
        .route("/api/pairings", get(super::pairings::pairings_list))
        .route(
            "/api/pairings/approve-code",
            post(super::pairings::pairing_approve_code),
        )
        .route(
            "/api/pairings/:id/approve",
            post(super::pairings::pairing_approve),
        )
        .route(
            "/api/pairings/:id/reject",
            post(super::pairings::pairing_reject),
        )
        .route("/api/watches", get(super::watches::watches_list))
        .route("/api/watches/:id/stop", post(super::watches::watch_stop))
        .route(
            "/api/dispatch/tasks/:task_id/retry",
            post(super::dispatch::dispatch_retry_task),
        )
        .route(
            "/api/dispatch/parents/:parent_id/retry",
            post(super::dispatch::dispatch_retry_parent),
        )
        .route(
            "/api/dispatch-config",
            get(super::dispatch_config::dispatch_config_get)
                .post(super::dispatch_config::dispatch_config_set),
        )
        .route(
            "/api/admin-permissions",
            get(admin_perms_get).post(admin_perms_set),
        )
        .route("/api/quicknotes", post(quicknotes_save))
        // Workspace file discovery + folder creation
        .route("/api/workspace/files", get(super::workspace::list_files))
        .route("/api/chat/files", get(super::workspace::mention_files))
        .route("/api/workspace/file", get(super::workspace::read_file))
        .route("/api/ws/terminal", get(super::terminal::ws_terminal))
        .route("/api/workspace/mkdir", post(super::workspace::mkdir))
        // Profile file editor — SOUL.md + MEMORY.md per agent folder
        .route(
            "/api/agents/:folder/files",
            get(super::profile_files::get_files).put(super::profile_files::put_files),
        )
        // Soul Core — the three global files describing the human and the
        // machine. Deliberately not under /api/space/apps/, which app_auth
        // gates per app id.
        .route(
            "/api/user-profile",
            get(super::user_profile::get_user_profile).put(super::user_profile::put_user_profile),
        )
        .route(
            "/api/tools-notes",
            get(super::user_profile::get_tools_notes).put(super::user_profile::put_tools_notes),
        )
        .route(
            "/api/agents-rules",
            get(super::user_profile::get_agents_rules).put(super::user_profile::put_agents_rules),
        )
        // Workflows (saved DAGs of agent + script steps)
        .route(
            "/api/workflows",
            get(super::workflow::workflows_list).post(super::workflow::workflows_def_create),
        )
        .route(
            "/api/workflows/draft",
            post(super::workflow::workflows_draft),
        )
        .route(
            "/api/workflows/settings",
            get(super::workflow::workflows_settings_get)
                .put(super::workflow::workflows_settings_put),
        )
        .route("/api/workflows/runs", get(super::workflow::workflows_runs))
        .route(
            "/api/workflows/runs/:id",
            get(super::workflow::workflows_run_get)
                .patch(super::workflow::workflows_run_rename)
                .delete(super::workflow::workflows_run_delete),
        )
        .route(
            "/api/workflows/runs/:id/cancel",
            post(super::workflow::workflows_run_cancel),
        )
        .route(
            "/api/workflows/runs/:id/activity",
            get(super::workflow::workflows_run_activity),
        )
        .route(
            "/api/workflows/:name/run",
            post(super::workflow::workflows_run_start),
        )
        .route(
            "/api/workflows/:name/definition",
            get(super::workflow::workflows_def_get)
                .put(super::workflow::workflows_def_update)
                .patch(super::workflow::workflows_def_patch)
                .delete(super::workflow::workflows_def_delete),
        )
        .route(
            "/api/workflows/:name",
            delete(super::workflow::workflows_def_delete),
        )
        // Cowork teams (multi-agent dispatch)
        .route(
            "/api/cowork/teams",
            get(super::cowork::list_teams).post(super::cowork::create_team),
        )
        .route(
            "/api/cowork/teams/:id",
            patch(super::cowork::update_team).delete(super::cowork::delete_team),
        )
        .route(
            "/api/cowork/teams/:id/members",
            put(super::cowork::update_team_member),
        )
        .route(
            "/api/cowork/teams/:id/members/:folder",
            delete(super::cowork::remove_team_member),
        )
        .route(
            "/api/cowork/teams/:id/tasks",
            get(super::cowork::list_team_tasks).post(super::cowork::create_team_task),
        )
        .route(
            "/api/cowork/teams/:id/workspace",
            get(super::cowork::browse_team_workspace),
        )
        .route(
            "/api/cowork/teams/:team_id/tasks/:task_id",
            patch(super::cowork::update_team_task).delete(super::cowork::delete_team_task),
        )
        .route(
            "/api/cowork/teams/from-template",
            post(super::cowork::create_from_template),
        )
        .route(
            "/api/cowork/templates",
            get(super::cowork::list_templates).post(super::cowork::create_template),
        )
        .route(
            "/api/cowork/templates/:id",
            put(super::cowork::update_template).delete(super::cowork::delete_template),
        )
        .route(
            "/api/cowork/teams/:id/save-as-template",
            post(super::cowork::save_team_as_template),
        )
        .route("/api/cowork/personas", get(super::cowork::list_personas))
        .route(
            "/api/cowork/personas/:name/file",
            get(super::cowork::get_persona_file).put(super::cowork::put_persona_file),
        )
        // LLM config (specific routes before parameterized)
        .route(
            "/api/llm-config",
            get(llm_config_list).post(llm_config_create),
        )
        .route("/api/llm-config/active", post(llm_config_set_active))
        .route("/api/llm-config/test", post(llm_config_test))
        .route("/api/llm-config/models", post(llm_config_fetch_models))
        .route(
            "/api/llm-config/:id",
            delete(llm_config_delete).patch(llm_config_update),
        )
        // OAuth sign-in for subscription providers (Claude Code / Codex /
        // Antigravity). Responses are token-free; see ui_server::oauth.
        .route(
            "/api/oauth/providers",
            get(super::oauth::oauth_providers_list),
        )
        .route(
            "/api/oauth/accounts",
            get(super::oauth::oauth_accounts_list),
        )
        .route(
            "/api/oauth/accounts/:id",
            delete(super::oauth::oauth_account_delete),
        )
        .route(
            "/api/oauth/accounts/:id/refresh",
            post(super::oauth::oauth_account_refresh),
        )
        .route("/api/oauth/bind", post(super::oauth::oauth_bind_config))
        .route(
            "/api/oauth/test-model",
            post(super::oauth::oauth_test_model),
        )
        .route(
            "/api/oauth/accounts/:id/models",
            get(super::oauth::oauth_account_models),
        )
        .route("/api/oauth/flows/:id", get(super::oauth::oauth_flow_status))
        // Parameterized last so it cannot shadow the literal routes above.
        .route(
            "/api/oauth/:provider/start",
            post(super::oauth::oauth_start),
        )
        // Ready-made API-key provider presets (free tiers).
        .route(
            "/api/provider-catalog",
            get(super::oauth::provider_catalog_list),
        )
        // Engines are runtimes now (docs/runtime-protocol.md §5): the daemon
        // links no inference code, so `/api/whisper`, `/api/tts`, `/api/ocr`
        // and (bar the control-plane routes just below) `/api/decision` are a
        // generic reverse proxy onto whichever runtime fills that slot — same
        // paths and bodies as the old in-daemon engines served, so no client
        // changes. `/api/runtimes/*` and `/api/local-models/*` are new
        // (§5.1/§5.3): install/select/supervise runtimes, and the GGUF/MLX
        // model library.
        .route("/api/whisper", any(runtime_proxy::proxy_whisper))
        .route("/api/whisper/*rest", any(runtime_proxy::proxy_whisper))
        .route("/api/tts", any(runtime_proxy::proxy_tts))
        .route("/api/tts/*rest", any(runtime_proxy::proxy_tts))
        .route("/api/ocr", any(runtime_proxy::proxy_ocr))
        .route("/api/ocr/*rest", any(runtime_proxy::proxy_ocr))
        // Typed decisions: the gate and the pre-turn skill router stay in the
        // daemon (control plane); everything else about `sen-sysone` (model
        // management, `/ask`, backend settings) is proxied. Static routes win
        // over the wildcard below them (axum/matchit: more specific first).
        .route("/api/decision/gate", get(decision_gate_get).put(decision_gate_put))
        .route("/api/decision/gate/check", post(decision_gate_check))
        .route("/api/decision/skills", get(decision_skills_get).put(decision_skills_put))
        .route("/api/decision/skills/check", post(decision_skills_check))
        .route(
            "/api/decision/settings",
            get(runtime_proxy::decision_settings_get).put(runtime_proxy::decision_settings_put),
        )
        .route("/api/decision", any(runtime_proxy::proxy_decision))
        .route("/api/decision/*rest", any(runtime_proxy::proxy_decision))
        // JEV v2.2 control plane (P0-P1): the file-based spec registry and
        // the neutral-v1 trace store. See `crate::control_plane`.
        .route("/api/control-plane/specs", get(specs_list))
        .route("/api/control-plane/specs/:id/mode", put(spec_mode_put))
        .route("/api/control-plane/decisions/replay", post(decisions_replay))
        .route("/api/traces", get(traces_list))
        .route("/api/traces/:id", get(trace_get))
        // Runtime management (§5.1).
        .route("/api/runtimes", get(runtime_rest::get_runtimes))
        .route("/api/runtimes/catalog", get(runtime_rest::get_catalog))
        .route("/api/runtimes/check-updates", post(runtime_rest::post_check_updates))
        .route("/api/runtimes/install", post(runtime_rest::post_install))
        .route("/api/runtimes/install-local", post(runtime_rest::post_install_local))
        .route("/api/runtimes/jobs", get(runtime_rest::get_jobs))
        .route("/api/runtimes/jobs/:job_id", get(runtime_rest::get_job))
        .route("/api/runtimes/jobs/:job_id/cancel", post(runtime_rest::post_job_cancel))
        .route("/api/runtimes/:id/versions/:version", delete(runtime_rest::delete_version))
        .route("/api/runtimes/selections", put(runtime_rest::put_selections))
        .route(
            "/api/runtimes/settings",
            get(runtime_rest::get_settings).put(runtime_rest::put_settings),
        )
        .route("/api/runtimes/slots/:slot/start", post(runtime_rest::post_slot_start))
        .route("/api/runtimes/processes/:key/stop", post(runtime_rest::post_process_stop))
        .route("/api/runtimes/:id/logs", get(runtime_rest::get_logs))
        // Local model library (§5.3) and the OpenAI-compatible model route (§5.4).
        .route("/api/local-models", get(local_models_rest::get_local_models))
        .route("/api/local-models/hf-files", get(local_models_rest::get_hf_files))
        .route("/api/local-models/download", post(local_models_rest::post_download))
        .route("/api/local-models/downloads", get(local_models_rest::get_downloads))
        .route("/api/local-models/downloads/:id", get(local_models_rest::get_download))
        .route("/api/local-models/downloads/:id/cancel", post(local_models_rest::post_download_cancel))
        .route("/api/local-models/:key", delete(local_models_rest::delete_model))
        .route("/api/local-models/:key/load", post(local_models_rest::post_load))
        .route("/api/local-models/:key/unload", post(local_models_rest::post_unload))
        .route(
            "/api/local-models/settings",
            get(local_models_rest::get_settings).put(local_models_rest::put_settings),
        )
        .route("/api/runtimes/models/:key/*rest", any(runtime_proxy::proxy_model))
        // Code executor REPL — sandboxed JS via senclaw-js engine.
        .route("/api/code/run", post(code_run))
        // Code artifacts — publish/browse/run reusable snippets.
        .route(
            "/api/code/artifacts",
            get(list_artifacts).post(create_artifact),
        )
        .route(
            "/api/code/artifacts/:id",
            get(get_artifact)
                .put(update_artifact)
                .delete(delete_artifact),
        )
        .route("/api/code/artifacts/:id/run", post(run_artifact))
        // Embedding provider config
        .route(
            "/api/embedding-config",
            get(embedding_config_get).post(embedding_config_save),
        )
        // Cognitive config
        .route(
            "/api/cognitive-config",
            get(super::cognitive_config::cognitive_config_get)
                .post(super::cognitive_config::cognitive_config_save),
        )
        // Wiki API
        .route("/api/wiki/tree", get(wiki_tree))
        .route(
            "/api/wiki/file",
            get(wiki_read).put(wiki_write).delete(wiki_file_delete),
        )
        .route("/api/wiki/search", get(wiki_search))
        .route("/api/wiki/stats", get(wiki_stats))
        .route("/api/wiki/history", get(wiki_history))
        .route("/api/wiki/tags", get(wiki_tags))
        .route("/api/wiki/mkdir", post(wiki_mkdir))
        .route(
            "/api/wiki/upload",
            post(wiki_upload).layer(DefaultBodyLimit::max(12 * 1024 * 1024)),
        )
        .route("/api/wiki/dir", delete(wiki_dir_delete))
        // MCP server management
        .route(
            "/api/mcp-servers",
            get(mcp_servers_list).post(mcp_servers_save),
        )
        .route(
            "/api/mcp-servers/:name",
            get(mcp_servers_get).delete(mcp_servers_delete),
        )
        .route("/api/mcp-servers/:name/connect", post(mcp_servers_connect))
        .route(
            "/api/mcp-servers/:name/disconnect",
            post(mcp_servers_disconnect),
        )
        .route("/api/mcp-servers/:name/tools", post(mcp_servers_tools))
        .route("/api/mcp-servers/:name/test", post(mcp_servers_test))
        .route("/api/mcp-servers/:name/enabled", post(mcp_servers_enabled))
        // MCP tool aliases (Plugins → Alias): rename or override tools
        .route(
            "/api/tool-aliases",
            get(super::tool_aliases::aliases_list).post(super::tool_aliases::aliases_create),
        )
        .route(
            "/api/tool-aliases/:alias",
            axum::routing::put(super::tool_aliases::aliases_update)
                .delete(super::tool_aliases::aliases_delete),
        )
        .route(
            "/api/tool-aliases/:alias/enabled",
            post(super::tool_aliases::aliases_set_enabled),
        )
        // Hooks config
        .route("/api/hooks", get(hooks_get).put(hooks_put))
        // ── Space API ─────────────────────────────────────────────────────────
        // Notes
        .route(
            "/api/space/notes",
            get(space_notes_list).post(space_notes_create),
        )
        .route("/api/space/notes/search", get(space_notes_search))
        .route(
            "/api/space/notes/:id",
            axum::routing::put(space_notes_update).delete(space_notes_delete),
        )
        // Calendar
        .route(
            "/api/space/calendar/events",
            get(space_events_list).post(space_events_create),
        )
        .route(
            "/api/space/calendar/events/search",
            get(space_events_search),
        )
        .route(
            "/api/space/calendar/events/:id",
            get(space_events_get)
                .patch(space_events_update)
                .delete(space_events_delete),
        )
        .route(
            "/api/space/calendar/events/:id/reminder",
            post(space_events_set_reminder),
        )
        .route("/api/space/calendar/today", get(space_today_summary))
        // Tray screen captures (read-only; written by the desktop tray)
        .route("/api/space/screenshots/:name", get(space_screenshot_get))
        // AI-fill a captured shot's note fields (vision, or OCR → text LLM)
        .route(
            "/api/space/screenshots/extract",
            post(space_screenshot_extract),
        )
        // Schedules
        .route(
            "/api/space/schedules",
            get(space_schedules_list).post(space_schedules_create),
        )
        .route(
            "/api/space/schedules/:id",
            get(space_schedules_detail)
                .patch(space_schedules_update)
                .delete(space_schedules_cancel),
        )
        .route(
            "/api/space/schedules/:id/run-now",
            post(space_schedules_run_now),
        )
        // Token usage accounting (llm_usage_log / llm_usage_daily / pricing).
        .route("/api/usage/overview", get(super::usage::usage_overview))
        .route("/api/usage/daily", get(super::usage::usage_daily))
        .route("/api/usage/breakdown", get(super::usage::usage_breakdown))
        .route("/api/usage/log", get(super::usage::usage_log))
        .route(
            "/api/usage/pricing",
            get(super::usage::pricing_list).put(super::usage::pricing_upsert),
        )
        .route(
            "/api/usage/pricing/:model",
            delete(super::usage::pricing_delete),
        )
        // Background tasks — autonomous work, no chat session. Distinct from
        // the schedules above, which run in a chat and reply to a human.
        .route(
            "/api/background/tasks",
            get(super::background::list).post(super::background::create),
        )
        .route(
            "/api/background/parse",
            post(super::background::parse_quick),
        )
        .route(
            "/api/background/tasks/:id",
            get(super::background::detail)
                .patch(super::background::update)
                .delete(super::background::delete),
        )
        .route(
            "/api/background/tasks/:id/run-now",
            post(super::background::run_now),
        )
        .route(
            "/api/background/tasks/:id/runs",
            get(super::background::runs),
        )
        .route(
            "/api/background/runs/:id",
            get(super::background::run_detail),
        )
        .route(
            "/api/background/runs/:id/cancel",
            post(super::background::cancel_run),
        )
        .route("/api/background/stats", get(super::background::stats))
        // Apps
        .route("/api/space/apps", get(space_apps_list))
        .route("/api/space/apps/status", get(space_apps_status))
        .route("/api/space/apps/updates", get(space_apps_updates))
        // Deliberately NOT under /api/space/apps/ — everything there is
        // scoped to one app id by `app_auth`, and this is the fleet-wide switch.
        .route(
            "/api/space/app-token-mode",
            get(space_app_token_mode_get).put(space_app_token_mode_put),
        )
        .route("/api/space/apps/register", post(space_apps_register))
        .route(
            "/api/space/apps/register-local",
            post(space_apps_register_local),
        )
        .route(
            "/api/space/apps/install-zip",
            // Server-app ZIPs (Next.js standalone) are tens of MB — raise the
            // default 2 MB body limit to the handler's 50 MB cap (+slack).
            post(space_apps_install_zip).layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/api/space/apps/:id/env", get(space_app_env))
        .route(
            "/api/space/apps/:id/runtime",
            get(super::space_runtime::space_app_runtime),
        )
        // Literal segment, so it wins over `:id` — same shape as the existing
        // `/api/space/apps/updates`.
        .route(
            "/api/space/apps/sandbox-overview",
            get(super::space_runtime::space_apps_sandbox_overview),
        )
        .route(
            "/api/space/apps/:id/sandbox",
            get(space_app_sandbox_get).put(space_app_sandbox_put),
        )
        .route("/api/space/apps/:id/config", get(space_app_config_list))
        .route(
            "/api/space/apps/:id/config/:key",
            get(space_app_config_get)
                .put(space_app_config_set)
                .delete(space_app_config_delete),
        )
        .route(
            "/api/space/apps/:id/sqlite/query",
            post(space_app_sqlite_query),
        )
        .route("/api/space/apps/:id/mcp", get(space_app_mcp_info))
        .route(
            "/api/space/apps/:id/mcp/register",
            post(space_app_mcp_register),
        )
        .route(
            "/api/space/apps/:id/logs",
            get(space_app_logs_get).delete(space_app_logs_clear),
        )
        .route("/api/space/apps/:id/bridge", post(space_apps_bridge))
        .route(
            "/api/space/apps/:id/token",
            get(space_app_token_get).post(space_app_token_rotate),
        )
        .route("/api/space/apps/:id/static/*path", get(space_apps_static))
        .route(
            "/api/space/apps/:id/proxy/*path",
            axum::routing::any(space_apps_proxy),
        )
        .route(
            "/api/space/apps/:id/proxy/",
            axum::routing::any(space_apps_proxy_root),
        )
        .route(
            "/api/space/apps/:id/proxy",
            axum::routing::any(space_apps_proxy_root),
        )
        .route("/api/space/apps/:id", delete(space_apps_delete))
        .route("/api/space/apps/:id/update", post(space_apps_update))
        .route("/api/space/apps/:id/restart", post(space_apps_restart))
        .route("/api/space/apps/:id/stop", post(space_apps_stop))
        .route("/api/space/apps/:id/start", post(space_apps_start))
        .route("/api/space/apps/:id/ready", get(space_apps_ready))
        .route(
            "/api/space/apps/:id/requirements",
            get(space_app_requirements),
        )
        // External sync
        .route(
            "/api/space/sync/google-calendar",
            post(space_sync_google_calendar),
        )
        .route(
            "/api/space/sync/google-workspace",
            post(space_sync_google_workspace),
        )
        .route(
            "/api/space/sync/apple-calendar",
            post(space_sync_apple_calendar),
        )
        .route("/api/space/sync/apple-notes", post(space_sync_apple_notes))
        .route("/api/space/sync/status", get(super::space::space_sync_status))
        // ── Chat interaction resolve (mobile parity with WS permission/question) ─
        .route(
            "/api/chat/permission/respond",
            post(chat_permission_respond),
        )
        .route("/api/chat/question/respond", post(chat_question_respond))
        .route("/api/chat/form/respond", post(chat_form_respond))
        .route("/api/chat/plan/respond", post(chat_plan_respond))
        // ── Chat sync for relay clients (delta history + agent-state snapshot) ──
        .route("/api/chat/history", get(chat_history))
        .route("/api/chat/states", get(chat_states))
        // Git worktrees created for isolated agent runs: review and land them.
        .route("/api/worktrees", get(super::worktrees::worktrees_list))
        .route("/api/worktrees/diff", get(super::worktrees::worktree_diff))
        .route("/api/worktrees/merge", post(super::worktrees::worktree_merge))
        .route("/api/worktrees/rebase", post(super::worktrees::worktree_rebase))
        .route("/api/worktrees/pr", post(super::worktrees::worktree_pr))
        .route("/api/worktrees/remove", post(super::worktrees::worktree_remove))
        // Language-server integration: status and settings.
        .route("/api/lsp/status", get(super::lsp::lsp_status))
        .route(
            "/api/lsp/settings",
            get(super::lsp::lsp_settings_get).put(super::lsp::lsp_settings_put),
        )
        // Code sessions: a chat pinned to one repository, as the mobile app
        // has called it since the old code engine was removed (see
        // ui_server/code_sessions.rs).
        .route(
            "/api/code/sessions",
            get(super::code_sessions::sessions_list).post(super::code_sessions::sessions_create),
        )
        .route(
            "/api/code/sessions/:id",
            get(super::code_sessions::sessions_get).delete(super::code_sessions::sessions_archive),
        )
        .route("/api/code/sessions/:id/files", get(super::code_sessions::sessions_files))
        .route(
            "/api/code/sessions/:id/file-content",
            get(super::code_sessions::sessions_file_content),
        )
        .route("/api/code/sessions/:id/git-log", get(super::code_sessions::sessions_git_log))
        .route("/api/code/sessions/:id/rollback", post(super::code_sessions::sessions_rollback))
        .route("/api/code/sessions/:id/chat", post(super::code_sessions::sessions_chat))
        .route(
            "/api/code/projects/:id/groups",
            get(super::code_sessions::projects_groups).post(super::code_sessions::projects_groups_create),
        )
        .route("/api/code/groups/:gid/messages", get(super::code_sessions::groups_messages))
        .route(
            "/api/code/groups/:gid/stop-current",
            post(super::code_sessions::groups_stop_current),
        )
        .route("/api/fs/ls", get(super::code_sessions::fs_ls))
        .route("/api/code/edit-stats", get(super::code_sessions::edit_stats))
        // Failure ledger: tool failures and what happened next (read-only).
        .route("/api/failures", get(super::failures::failures_list))
        .route(
            "/api/failures/summary",
            get(super::failures::failures_summary),
        )
        // Trajectory log: recorded turns for replay and evals.
        .route(
            "/api/chats/:jid/trajectory",
            get(super::trajectory::trajectory_list).delete(super::trajectory::trajectory_delete),
        )
        .route(
            "/api/chats/:jid/trajectory/settings",
            put(super::trajectory::trajectory_settings),
        )
        .route(
            "/api/chats/:jid/trajectory/:turn",
            get(super::trajectory::trajectory_turn),
        )
        // Shadow-git checkpoints: list / diff / restore / explain per chat.
        .route(
            "/api/chats/:jid/checkpoints",
            get(super::checkpoints::checkpoints_list),
        )
        .route(
            "/api/chats/:jid/checkpoints/settings",
            put(super::checkpoints::checkpoints_settings),
        )
        .route(
            "/api/chats/:jid/checkpoints/:id/diff",
            get(super::checkpoints::checkpoint_diff),
        )
        .route(
            "/api/chats/:jid/checkpoints/:id/restore",
            post(super::checkpoints::checkpoint_restore),
        )
        .route(
            "/api/chats/:jid/checkpoints/:id/explain",
            post(super::checkpoints::checkpoint_explain),
        )
        .route(
            "/api/chats/:jid/checkpoints/:id/document",
            post(super::checkpoints::checkpoint_document),
        )
        // Workbench reverse ops (artifacts published by tools)
        .route(
            "/api/workbench/:jid/:id/mark-viewed",
            post(super::workbench::workbench_mark_viewed),
        )
        .route(
            "/api/workbench/:jid/:id/close",
            post(super::workbench::workbench_close),
        )
        .route(
            "/api/workbench/:jid/:id/read-file",
            get(super::workbench::workbench_read_file),
        )
        .route(
            "/api/workbench/:jid/:id/logs",
            get(super::workbench::workbench_fetch_logs),
        )
        // Cognitive memory (graph + Hebbian)
        .route(
            "/api/cognitive/stats",
            get(super::cognitive::cognitive_stats),
        )
        .route(
            "/api/cognitive/spaces",
            get(super::cognitive::cognitive_spaces),
        )
        .route(
            "/api/cognitive/nodes",
            get(super::cognitive::cognitive_list_nodes),
        )
        .route(
            "/api/cognitive/node/:id",
            get(super::cognitive::cognitive_get_node).delete(super::cognitive::cognitive_forget),
        )
        .route(
            "/api/cognitive/node/:id/re-extract",
            post(super::cognitive::cognitive_re_extract),
        )
        .route(
            "/api/cognitive/re-extract-pending",
            post(super::cognitive::cognitive_re_extract_pending),
        )
        .route(
            "/api/cognitive/decay-log",
            get(super::cognitive::cognitive_decay_log),
        )
        .route(
            "/api/cognitive/search",
            post(super::cognitive::cognitive_search),
        )
        .route(
            "/api/cognitive/recall",
            post(super::cognitive::cognitive_recall),
        )
        .route("/api/cognitive/add", post(super::cognitive::cognitive_add))
        .route(
            "/api/cognitive/upload",
            post(super::cognitive::cognitive_upload).layer(DefaultBodyLimit::max(12 * 1024 * 1024)),
        )
        .route(
            "/api/cognitive/subgraph",
            get(super::cognitive::cognitive_subgraph),
        )
        .route(
            "/api/cognitive/top-nodes",
            get(super::cognitive::cognitive_top_nodes),
        )
        .route(
            "/api/cognitive/history",
            get(super::cognitive::cognitive_history),
        )
        .route(
            "/api/cognitive/predicates",
            get(super::cognitive::cognitive_predicates)
                .post(super::cognitive::cognitive_set_predicate),
        )
        .route(
            "/api/cognitive/sample",
            get(super::cognitive::cognitive_sample),
        )
        .route(
            "/api/cognitive/full-graph",
            get(super::cognitive::cognitive_full_graph),
        )
        .route(
            "/api/cognitive/cleanup",
            post(super::cognitive::cognitive_cleanup),
        )
        .route(
            "/api/cognitive/maintenance",
            post(super::cognitive::cognitive_maintenance),
        )
        // Embedding model management
        .route(
            "/api/embedding/features",
            get(super::embedding_models::embedding_features),
        )
        .route(
            "/api/embedding/models",
            get(super::embedding_models::embedding_list_models),
        )
        .route(
            "/api/embedding/download-model",
            post(super::embedding_models::embedding_download_model),
        )
        // Static files. ServeDir handles real assets (/assets/*, favicon,
        // etc.); paths it can't resolve fall through to the SPA fallback
        // which serves the right HTML shell with HTTP 200.
        .nest_service("/", serve_dir)
        // SPA fallback — must return 200 so React-Router can take over.
        .fallback(get(move |uri: axum::http::Uri| {
            spa_fallback(dist_dir.clone(), uri)
        }))
        // Loopback-origin allowlist. The old `CorsLayer::permissive()` here
        // (ACAO `*`) let any web page the user visited read API responses off
        // the loopback daemon — /api/llm-config serves cleartext provider
        // keys. See auth::restrictive_cors.
        .layer(super::auth::restrictive_cors())
        // Per-app access tokens: an app may only act on its own
        // /api/space/apps/<id>/… . Layered here rather than at the serve site
        // so the relay bridge — which reuses this router — is scoped too.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            super::app_auth::app_auth_mw,
        ))
        .with_state(state)
}

// ===== App error type =====

pub struct AppError(pub StatusCode, pub String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.1 });
        (self.0, Json(body)).into_response()
    }
}

// ===== Server launcher =====

/// Start the UI HTTP server on the configured port. Binds to
/// `ui_server.bind_host` (default 127.0.0.1); non-loopback binds are token-
/// gated by the auth middleware.
pub async fn start_ui_server(state: Arc<UiState>, port: u16) -> Result<()> {
    let host = state.config.ui_server.bind_host.clone();
    let api_auth = Arc::clone(&state.api_auth);
    let router = build_router(state).layer(axum::middleware::from_fn_with_state(
        api_auth,
        super::auth::http_auth_mw,
    ));
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!("[UIServer] Web UI at http://{addr}");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

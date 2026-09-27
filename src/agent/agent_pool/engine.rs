//! ZenCoreApi — production [`CoreApi`] backed by [`ZenEngine`] (the zen-core runtime).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use super::traits::{AgentToolInfo, CoreApi, CoreHandlers};
use super::types::{
    AskQuestionRequestData, CompactExecData, CompactStartData, MessageCompleteData,
    SessionErrorData, StateUpdateData, TodosUpdateItem, ToolPermissionRequestData, MAIN_AGENT_ID,
};
use crate::agent::permission_bridge::{AskQuestionData, AskQuestionOption};
use crate::config::Config;
use crate::mcp::helper::McpServerConfig;
use crate::types::GroupBinding;
use crate::zen_core::{
    AskQuestionResponseData, EngineEvent, FormResponseData, PlanExitResponseData, SessionState,
    ToolPermissionResponseData, ZenCore, ZenCoreOptions, ZenEngine,
};
use tokio::sync::broadcast::error::RecvError;

/// Production [`CoreApi`] backed by [`ZenEngine`] (the zen-core runtime).
///
/// Manages one [`ZenEngine`] per JID, bridges engine events to CoreApi
/// handler callbacks, and delegates lifecycle operations to the engine.
pub struct ZenCoreApi {
    engines: Mutex<HashMap<String, Arc<ZenEngine>>>,
    handlers: Arc<Mutex<HashMap<String, CoreHandlers>>>,
    mcp_manager: Option<Arc<crate::mcp::manager::McpManager>>,
    /// Optional WorkbenchBridge — when set, `ensure_engine` binds each new
    /// engine's event stream so artifacts surface in the UI / IM fallback.
    workbench_bridge: Mutex<Option<Arc<crate::agent::workbench_bridge::WorkbenchBridge>>>,
    /// Per-jid bot tokens used by the workbench-bridge IM fallback.
    bot_tokens: Mutex<HashMap<String, Option<String>>>,
    /// Per-jid LLM override (entry id in the global `llmConfigs` list). Cached so
    /// a lazily created engine picks up the group's model in `ensure_engine`.
    model_overrides: Mutex<HashMap<String, String>>,
    /// Per-jid working directory. Cached so a lazily created engine picks up the
    /// group's workspace dir in `ensure_engine`. Without this the engine keeps
    /// the empty `ZenCoreOptions` default and every Bash spawn fails with ENOENT
    /// (`current_dir("")`), so the whole "code" feature is dead on a fresh chat.
    working_dirs: Mutex<HashMap<String, String>>,
    /// Callback invoked when an engine emits a plan-exit request. Caller
    /// (lib.rs) uses it to broadcast the event over WS so the UI can render
    /// the plan-approval modal. `Arc<Mutex>` so spawned event loops can hold
    /// a cheap clone and pick up callbacks set after engine creation.
    on_plan_exit_request:
        Arc<Mutex<Option<Arc<dyn Fn(String, crate::zen_core::PlanExitRequestData) + Send + Sync>>>>,
    /// Callback fired for every `EngineEvent::ToolExecutionComplete` and
    /// `ToolExecutionError`. Lets `lib.rs` push a `tool:execution` WS event so
    /// the chat UI can render a claude-code-style collapsible "Read 3 files,
    /// ran 1 command" tool-group card.
    on_tool_execution: Arc<Mutex<Option<Arc<dyn Fn(String, ToolExecutionEvent) + Send + Sync>>>>,
    /// Callback fired for every `EngineEvent::WidgetEmit`. Lets `lib.rs`
    /// persist + push a one-way `chat:widget` WS frame so the chat UI renders
    /// the inline widget card. Mirrors `on_tool_execution` (one-way, no
    /// response round-trip).
    on_widget_emit:
        Arc<Mutex<Option<Arc<dyn Fn(String, crate::zen_core::WidgetEmitData) + Send + Sync>>>>,
    /// Token accounting sink. When set, every `EngineEvent::LlmUsage` an
    /// engine emits (agent / subagent / compact / hook calls) is recorded with
    /// this engine's jid. `Arc<Mutex>` for the same late-wiring reason as the
    /// callbacks above.
    usage_recorder: Arc<Mutex<Option<Arc<crate::usage::UsageRecorder>>>>,
    /// The admin "skip approval" choice, applied to every engine this API
    /// creates from now on.
    ///
    /// `update_skip_permissions` only reaches an engine that already exists,
    /// and `ensure_engine` builds its options from `Default`, whose flags are
    /// false. So the saved choice used to apply to live chats only: after a
    /// restart every *new* chat asked again, and a headless daemon has no UI
    /// to answer — the agent stopped at its first tool and waited out its
    /// timeout.
    default_skip_permissions: std::sync::atomic::AtomicBool,
}

/// Wire-format tool-execution event used by the AgentPool → WS gateway path.
/// `ok = true` for `ToolExecutionComplete`, `ok = false` for `Error`.
#[derive(Debug, Clone)]
pub struct ToolExecutionEvent {
    pub agent_id: String,
    pub tool_name: String,
    pub title: String,
    pub summary: String,
    /// The model's own words for what this step does (`Bash`/`Task`
    /// `description`). Empty means the client falls back to the tool's verb.
    pub description: String,
    pub content: serde_json::Value,
    pub ok: bool,
}

impl ZenCoreApi {
    pub fn new(mcp_manager: Option<Arc<crate::mcp::manager::McpManager>>) -> Self {
        Self {
            engines: Mutex::new(HashMap::new()),
            handlers: Arc::new(Mutex::new(HashMap::new())),
            mcp_manager,
            workbench_bridge: Mutex::new(None),
            bot_tokens: Mutex::new(HashMap::new()),
            model_overrides: Mutex::new(HashMap::new()),
            working_dirs: Mutex::new(HashMap::new()),
            on_plan_exit_request: Arc::new(Mutex::new(None)),
            on_tool_execution: Arc::new(Mutex::new(None)),
            on_widget_emit: Arc::new(Mutex::new(None)),
            usage_recorder: Arc::new(Mutex::new(None)),
            default_skip_permissions: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Set the skip-approval default inherited by engines created later.
    /// Live engines are updated separately, per jid.
    pub fn set_default_skip_permissions(&self, skip: bool) {
        self.default_skip_permissions
            .store(skip, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn default_skip_permissions_for_test(&self) -> bool {
        self.default_skip_permissions
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Wire a callback fired when any engine emits `EngineEvent::PlanExitRequest`.
    /// Used by lib.rs to broadcast `plan:exit:request` over the WebSocket gateway.
    pub fn set_on_plan_exit_request(
        &self,
        cb: Arc<dyn Fn(String, crate::zen_core::PlanExitRequestData) + Send + Sync>,
    ) {
        *self.on_plan_exit_request.lock().unwrap() = Some(cb);
    }

    /// Wire a callback fired for every tool execution (complete or error).
    /// Used by lib.rs to broadcast `tool:execution` over the WebSocket gateway
    /// so the chat UI can render tool-call activity inline.
    pub fn set_on_tool_execution(&self, cb: Arc<dyn Fn(String, ToolExecutionEvent) + Send + Sync>) {
        *self.on_tool_execution.lock().unwrap() = Some(cb);
    }

    /// Wire the token-accounting sink (called once from `run_daemon`).
    pub fn set_usage_recorder(&self, rec: Arc<crate::usage::UsageRecorder>) {
        *self.usage_recorder.lock().unwrap() = Some(rec);
    }

    /// Wire a callback fired for every `EngineEvent::WidgetEmit`. Used by
    /// lib.rs to persist the widget and broadcast a one-way `chat:widget`
    /// frame over the WebSocket gateway.
    pub fn set_on_widget_emit(
        &self,
        cb: Arc<dyn Fn(String, crate::zen_core::WidgetEmitData) + Send + Sync>,
    ) {
        *self.on_widget_emit.lock().unwrap() = Some(cb);
    }

    /// Inject the WorkbenchBridge so future-created engines emit artifact
    /// events into the bridge. Idempotent — last setter wins.
    pub fn set_workbench_bridge(
        &self,
        bridge: Arc<crate::agent::workbench_bridge::WorkbenchBridge>,
    ) {
        *self.workbench_bridge.lock().unwrap() = Some(bridge);
    }

    /// Update the cached `bot_token` for a JID. AgentPool calls this when
    /// loading group bindings so the workbench-bridge IM fallback can target
    /// the right bot when artifacts are published.
    pub fn set_bot_token(&self, jid: &str, bot_token: Option<String>) {
        self.bot_tokens
            .lock()
            .unwrap()
            .insert(jid.to_string(), bot_token);
    }

    fn with_handlers<F: FnOnce(&mut CoreHandlers)>(&self, jid: &str, f: F) {
        let mut map = self.handlers.lock().unwrap();
        let entry = map.entry(jid.to_string()).or_default();
        f(entry);
    }

    /// Create or retrieve the engine for a JID.
    fn ensure_engine(&self, jid: &str) -> Arc<ZenEngine> {
        let mut engines = self.engines.lock().unwrap();
        if let Some(engine) = engines.get(jid) {
            return engine.clone();
        }
        // Seed the working dir from the per-jid cache (populated by
        // `set_working_dir` before the engine is lazily created). Fall back to a
        // valid directory rather than the empty default — an empty `working_dir`
        // makes every Bash `current_dir("")` spawn fail with ENOENT, which breaks
        // the code feature entirely.
        let working_dir = self
            .working_dirs
            .lock()
            .unwrap()
            .get(jid)
            .cloned()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(default_working_dir);
        let skip = self
            .default_skip_permissions
            .load(std::sync::atomic::Ordering::Relaxed);
        let opts = ZenCoreOptions {
            instance_id: jid.to_string(),
            model_config_id: self.model_overrides.lock().unwrap().get(jid).cloned(),
            working_dir,
            // Inherit the admin choice at construction. Setting it afterwards
            // is too late: the engine's first tool call can happen before any
            // per-jid update reaches it.
            skip_file_edit_permission: skip,
            skip_bash_exec_permission: skip,
            skip_skill_permission: skip,
            skip_mcp_tool_permission: skip,
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, self.mcp_manager.clone());
        engine.initialize_plugins();
        // Refresh MCP bridge tools in the background
        {
            let engine = engine.clone();
            tokio::spawn(async move {
                engine.refresh_mcp_tools().await;
            });
        }
        // Bind WorkbenchBridge if wired (relays artifact events to UI + IM fallback).
        if let Some(bridge) = self.workbench_bridge.lock().unwrap().clone() {
            let bot_token = self.bot_tokens.lock().unwrap().get(jid).cloned().flatten();
            bridge.bind_engine(engine.clone(), jid, bot_token);
        }
        engines.insert(jid.to_string(), engine.clone());
        drop(engines);
        // Subscribe the event-bus forwarder exactly ONCE per engine — here, at
        // creation. Previously this lived in `create_session`, which AgentPool
        // calls more than once for the same jid (bind_group + stop_agent). Each
        // call spawned ANOTHER subscriber on the SAME cached engine's event bus,
        // so every think / skill / reply event was forwarded twice and the Web
        // UI rendered everything twice. Tying the bridge to engine creation makes
        // it 1:1 with the engine: a reused engine never double-bridges, and a
        // destroyed + recreated engine (the old loop sees `Closed` and exits)
        // re-bridges cleanly.
        self.bridge_events(jid, &engine);
        engine
    }

    /// Subscribe to the engine's EventBus and forward events to handlers.
    /// Called exactly once per engine, from `ensure_engine` at creation time.
    fn bridge_events(&self, jid: &str, engine: &Arc<ZenEngine>) {
        let jid = jid.to_string();
        let handlers_map = Arc::clone(&self.handlers);
        // Snapshot the plan-exit callback shared Mutex so the spawned loop
        // can fire it without re-locking through `&self`.
        let plan_callback_for_loop = self.on_plan_exit_request.clone();
        let tool_exec_callback_for_loop = self.on_tool_execution.clone();
        let widget_callback_for_loop = self.on_widget_emit.clone();
        let usage_recorder_for_loop = self.usage_recorder.clone();
        let mut rx = engine.event_bus.subscribe();

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        // Accounting first — must run even when no UI handlers
                        // are registered for this jid (background sessions,
                        // early boot), so it sits before the handlers gate.
                        if let EngineEvent::LlmUsage(d) = &event {
                            // Trace neutral-v1 (control plane, §16): always on,
                            // metadata only. `LlmUsage` never reaches the
                            // trajectory/failures line below (it `continue`s
                            // here unconditionally), so this is the only place
                            // that can record a turn's token/cache counts.
                            crate::control_plane::trace::record(&jid, &event);
                            let rec = usage_recorder_for_loop.lock().unwrap().clone();
                            if let Some(rec) = rec {
                                rec.record(
                                    crate::usage::UsageEvent {
                                        jid: jid.clone(),
                                        agent_id: d.agent_id.clone(),
                                        session_id: d.session_id.clone(),
                                        profile: d.profile.clone(),
                                        provider: d.provider.clone(),
                                        model: d.model.clone(),
                                        latency_ms: d.latency_ms,
                                        ok: d.ok,
                                        ..crate::usage::UsageEvent::new(
                                            crate::usage::UsageSource::from_zen(&d.source),
                                        )
                                    }
                                    .with_tokens(&d.usage),
                                );
                            }
                            continue;
                        }
                        let handlers = handlers_map.lock().unwrap().get(&jid).cloned();
                        let h = match handlers {
                            Some(h) => h,
                            None => continue,
                        };
                        // Trajectory side channel: a no-op unless recording is
                        // on for this chat (see `crate::trajectory`).
                        crate::trajectory::record(&jid, &event);
                        // Failure ledger: records tool failures and what
                        // happened next. Always on, writes only on failure and
                        // its outcome (see `crate::failures`).
                        crate::failures::record(&jid, &event);
                        // Trace neutral-v1: always on, metadata only (see
                        // `crate::control_plane::trace`).
                        crate::control_plane::trace::record(&jid, &event);
                        // Workspace mirror (progress.md/todo.json): a no-op
                        // unless `controlPlane.workspace.enabled` (default on)
                        // — never changes what a tool returns to the LLM, only
                        // mirrors it to disk (see `crate::control_plane::workspace`).
                        let cp_settings = crate::gateway::group_manager::load_control_plane_settings(
                            &crate::control_plane::default_config_path(),
                        );
                        crate::control_plane::workspace::record(&cp_settings, &jid, &event);
                        match event {
                            EngineEvent::MessageComplete(data) => {
                                if let Some(ref cb) = h.message_complete {
                                    cb(MessageCompleteData {
                                        agent_id: data.agent_id,
                                        reasoning: data.reasoning,
                                        content: data.content,
                                        has_tool_calls: data.has_tool_calls,
                                        output_tokens: data.output_tokens,
                                    });
                                }
                            }
                            EngineEvent::TextChunk(data) => {
                                // Sub-agent text is not the user-facing reply —
                                // streaming it would interleave a dispatched
                                // worker's prose into the chat bubble.
                                if data.agent_id == MAIN_AGENT_ID {
                                    if let Some(ref cb) = h.text_chunk {
                                        cb(data);
                                    }
                                }
                            }
                            EngineEvent::StateUpdate(data) => {
                                if let Some(ref cb) = h.state_update {
                                    cb(StateUpdateData {
                                        state: data.state.as_str().to_string(),
                                    });
                                }
                            }
                            EngineEvent::TodosUpdate(items) => {
                                tracing::info!(
                                    "[AgentPool] bridge_events TodosUpdate jid={jid} items={}",
                                    items.len()
                                );
                                if let Some(ref cb) = h.todos_update {
                                    cb(items
                                        .iter()
                                        .map(|item| TodosUpdateItem {
                                            content: item.content.clone(),
                                            status: item.status.clone(),
                                            active_form: item.active_form.clone(),
                                        })
                                        .collect());
                                } else {
                                    tracing::warn!(
                                        "[AgentPool] bridge_events TodosUpdate for {jid} but NO handler registered"
                                    );
                                }
                            }
                            EngineEvent::CompactStart(_) => {
                                if let Some(ref cb) = h.compact_start {
                                    cb(CompactStartData);
                                }
                            }
                            EngineEvent::CompactExec(d) => {
                                if let Some(ref cb) = h.compact_exec {
                                    cb(CompactExecData {
                                        summary: d.summary.clone(),
                                    });
                                }
                            }
                            EngineEvent::SessionError(data) => {
                                if let Some(ref cb) = h.session_error {
                                    cb(SessionErrorData {
                                        code: data.error.code,
                                        message: data.error.message,
                                    });
                                }
                            }
                            EngineEvent::ToolPermissionRequest(data) => {
                                if let Some(ref cb) = h.tool_permission_request {
                                    cb(ToolPermissionRequestData {
                                        tool_name: data.tool_name,
                                        permission_key: data.permission_key,
                                        title: data.title,
                                        content: data.content,
                                        options: data.options,
                                    });
                                }
                            }
                            EngineEvent::AskQuestionRequest(data) => {
                                if let Some(ref cb) = h.ask_question_request {
                                    cb(AskQuestionRequestData {
                                        agent_id: data.agent_id,
                                        questions: data
                                            .questions
                                            .into_iter()
                                            .map(|q| AskQuestionData {
                                                question: q.question,
                                                header: q.header,
                                                options: q
                                                    .options
                                                    .into_iter()
                                                    .map(|o| AskQuestionOption {
                                                        label: o.label,
                                                        description: o.description,
                                                    })
                                                    .collect(),
                                                multi_select: q.multi_select,
                                            })
                                            .collect(),
                                    });
                                }
                            }
                            EngineEvent::FormRequest(data) => {
                                if let Some(ref cb) = h.form_request {
                                    cb(data);
                                }
                            }
                            EngineEvent::WidgetEmit(data) => {
                                // One-way: forward to the global widget callback
                                // (persist + broadcast). No CoreHandlers entry —
                                // mirrors the ToolExecution path, not FormRequest.
                                let cb_opt = widget_callback_for_loop.lock().unwrap().clone();
                                if let Some(cb) = cb_opt {
                                    cb(jid.clone(), data);
                                }
                            }
                            EngineEvent::ConversationUsage(data) => {
                                if let Some(ref cb) = h.conversation_usage {
                                    cb(data);
                                }
                            }
                            EngineEvent::PlanExitRequest(data) => {
                                // Independent of CoreHandlers — uses the global
                                // ZenCoreApi callback wired at startup. Forwards
                                // to lib.rs which broadcasts the WS event so the
                                // UI can render the PlanExitDialog.
                                let cb_opt = plan_callback_for_loop.lock().unwrap().clone();
                                if let Some(cb) = cb_opt {
                                    cb(jid.clone(), data);
                                }
                            }
                            EngineEvent::ToolExecutionComplete(data) => {
                                let cb_opt = tool_exec_callback_for_loop.lock().unwrap().clone();
                                if let Some(cb) = cb_opt {
                                    cb(
                                        jid.clone(),
                                        ToolExecutionEvent {
                                            agent_id: data.agent_id,
                                            tool_name: data.tool_name,
                                            title: data.title,
                                            summary: data.summary,
                                            description: data.description,
                                            content: data.content,
                                            ok: true,
                                        },
                                    );
                                }
                            }
                            EngineEvent::ToolExecutionError(data) => {
                                let cb_opt = tool_exec_callback_for_loop.lock().unwrap().clone();
                                if let Some(cb) = cb_opt {
                                    cb(
                                        jid.clone(),
                                        ToolExecutionEvent {
                                            agent_id: data.agent_id,
                                            tool_name: data.tool_name,
                                            title: data.title,
                                            summary: String::new(),
                                            description: data.description,
                                            content: serde_json::Value::String(data.content),
                                            ok: false,
                                        },
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("[ZenCoreApi] event bus lagged by {} for {}", n, jid);
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }
}

impl ZenCoreApi {
    /// The working directory currently pinned for `jid`, if any. Reads the
    /// same per-jid cache `set_working_dir` fills, so it answers for chats
    /// whose engine has not been created yet. Used by checkpoints to know
    /// which tree a tool just wrote into.
    pub fn working_dir_for(&self, jid: &str) -> Option<String> {
        self.working_dirs
            .lock()
            .unwrap()
            .get(jid)
            .cloned()
            .filter(|d| !d.is_empty())
    }
}

impl CoreApi for ZenCoreApi {
    fn process_message(&self, jid: &str, prompt: &str, _group: &GroupBinding) -> Result<String> {
        let engine = self.ensure_engine(jid);
        engine.process_user_input(prompt, None)?;
        Ok("Dispatched to zen-core".to_string())
    }

    fn destroy_agent(&self, jid: &str) {
        let engine = self.engines.lock().unwrap().remove(jid);
        if let Some(e) = engine {
            e.dispose();
        }
        self.handlers.lock().unwrap().remove(jid);
    }

    fn set_use_tools(&self, jid: &str, tools: Vec<String>) {
        let engine = self.ensure_engine(jid);
        engine.set_use_tools(tools);
    }

    fn update_skip_permissions(&self, jid: &str, skip: bool) {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.update_skip_permissions(skip);
        }
    }

    fn set_default_skip_permissions(&self, skip: bool) {
        ZenCoreApi::set_default_skip_permissions(self, skip);
    }

    fn update_thinking(&self, jid: &str, enabled: bool) {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.update_thinking(enabled);
        }
    }

    fn set_pre_trigger_skill(&self, jid: &str, enabled: bool) {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.set_pre_trigger_skill(enabled);
        }
    }

    fn skill_cards(&self, jid: &str) -> Vec<crate::skills::matching::SkillCard> {
        self.engines
            .lock()
            .unwrap()
            .get(jid)
            .map(|engine| engine.skill_cards())
            .unwrap_or_default()
    }

    fn set_skill_route(&self, jid: &str, route: Option<crate::skills::matching::SkillRoute>) {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.set_skill_route(route);
        }
    }

    fn set_after_process(&self, jid: &str, enabled: bool) {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.set_after_process(enabled);
        }
    }

    fn set_user_defaults(&self, jid: &str, block: Option<String>) {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.set_user_defaults(block);
        }
    }

    fn set_model_override(&self, jid: &str, id: Option<String>) {
        // Remember for lazily created engines.
        match &id {
            Some(v) => {
                self.model_overrides
                    .lock()
                    .unwrap()
                    .insert(jid.to_string(), v.clone());
            }
            None => {
                self.model_overrides.lock().unwrap().remove(jid);
            }
        }
        // Apply live if the engine already exists.
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.set_model_override(id);
        }
    }

    fn set_working_dir(&self, jid: &str, dir: &str) {
        // Cache first so a not-yet-created engine picks the dir up in
        // `ensure_engine` (mirrors the `model_overrides` pattern). Ignore empty
        // dirs — they would re-introduce the `current_dir("")` ENOENT bug.
        if !dir.is_empty() {
            self.working_dirs
                .lock()
                .unwrap()
                .insert(jid.to_string(), dir.to_string());
        }
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.set_working_dir(dir);
        }
    }

    fn clear_working_dir(&self, jid: &str) {
        self.working_dirs.lock().unwrap().remove(jid);
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.clear_working_dir();
        }
    }

    fn pause_session(&self, jid: &str) {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.pause_session();
        }
    }

    fn interrupt_session(&self, jid: &str) {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.interrupt_session(SessionState::Idle);
        }
    }

    fn reload_skills(&self, disabled: &[String]) {
        for engine in self.engines.lock().unwrap().values() {
            engine.reload_skills(disabled);
        }
    }

    fn reload_hooks(&self) {
        for engine in self.engines.lock().unwrap().values() {
            engine.reload_hooks();
        }
    }

    fn set_runtime_config(&self, _cfg: Arc<Config>) {
        // Config is passed via environment; no-op for zen-core
    }

    fn add_or_update_mcp_server(&self, jid: &str, cfg: &McpServerConfig) -> Result<()> {
        let engine = self.ensure_engine(jid);
        let zc_cfg = crate::zen_core::McpServerConfig {
            name: cfg.name.clone(),
            command: cfg.command.clone(),
            args: cfg.args.clone(),
            env: cfg.env.clone(),
            request_timeout_secs: None,
        };
        engine.add_or_update_mcp_server(&zc_cfg, "project")?;
        Ok(())
    }

    fn create_session(&self, jid: &str) -> Result<()> {
        // Do NOT bridge events here — `ensure_engine` already subscribes the
        // forwarder exactly once per engine. AgentPool calls create_session
        // more than once per jid (bind_group + stop_agent), so bridging here
        // would spawn a duplicate subscriber and double every emitted event.
        let engine = self.ensure_engine(jid);
        engine.create_session(None)?;
        Ok(())
    }

    fn get_tool_infos(&self, jid: &str) -> Vec<AgentToolInfo> {
        let engine = self.engines.lock().unwrap().get(jid).cloned();
        let Some(engine) = engine else {
            return Vec::new();
        };
        engine
            .get_tool_infos()
            .into_iter()
            .map(|t| AgentToolInfo {
                name: t.name,
                description: t.description,
                status: t.status,
            })
            .collect()
    }

    fn process_user_input(&self, jid: &str, prompt: &str) -> Result<()> {
        let engine = self.ensure_engine(jid);
        engine.process_user_input(prompt, None)?;
        Ok(())
    }

    fn process_user_input_with_images(
        &self,
        jid: &str,
        prompt: &str,
        images: Vec<crate::zen_core::ImageSource>,
    ) -> Result<()> {
        let engine = self.ensure_engine(jid);
        engine.process_user_input_with_images(prompt, None, images)?;
        Ok(())
    }

    fn queue_input_if_processing(&self, jid: &str, prompt: &str) -> bool {
        // No ensure_engine: if no engine exists yet, nothing is processing.
        let engine = self.engines.lock().unwrap().get(jid).cloned();
        match engine {
            Some(engine) => engine.queue_input_if_processing(prompt),
            None => false,
        }
    }

    fn has_session_tool_results(&self, jid: &str) -> bool {
        self.engines
            .lock()
            .unwrap()
            .get(jid)
            .map(|e| e.has_session_tool_results())
            .unwrap_or(false)
    }

    fn update_agent_mode(&self, jid: &str, mode: &str) {
        use crate::zen_core::AgentMode;
        let parsed = match mode {
            "Plan" => AgentMode::Plan,
            "Agent" => AgentMode::Agent,
            "Dag" => AgentMode::Dag,
            other => {
                tracing::warn!(
                    "[ZenCoreApi] update_agent_mode: unknown mode '{other}' for {jid}, ignored"
                );
                return;
            }
        };
        if let Some(engine) = self.engines.lock().unwrap().get(jid).cloned() {
            engine.update_agent_mode(parsed);
        } else {
            tracing::warn!("[ZenCoreApi] update_agent_mode: no engine for {jid}");
        }
    }

    fn get_agent_mode(&self, jid: &str) -> Option<String> {
        self.engines
            .lock()
            .unwrap()
            .get(jid)
            .map(|e| e.options.read().unwrap().agent_mode.as_str().to_string())
    }

    fn register_tools(&self, jid: &str, tools: Vec<std::sync::Arc<dyn crate::zen_core::Tool>>) {
        if let Some(engine) = self.engines.lock().unwrap().get(jid).cloned() {
            let count = tools.len();
            engine.register_tools(tools);
            tracing::info!("[ZenCoreApi] registered {count} dispatch tool(s) for {jid}");
        }
    }

    fn on_message_complete(
        &self,
        jid: &str,
        handler: Box<dyn Fn(MessageCompleteData) + Send + Sync>,
    ) {
        self.with_handlers(jid, |entry| {
            entry.message_complete = Some(Arc::from(handler));
        });
    }

    fn on_state_update(&self, jid: &str, handler: Box<dyn Fn(StateUpdateData) + Send + Sync>) {
        self.with_handlers(jid, |entry| {
            entry.state_update = Some(Arc::from(handler));
        });
    }

    fn on_text_chunk(
        &self,
        jid: &str,
        handler: Box<dyn Fn(crate::zen_core::TextChunkData) + Send + Sync>,
    ) {
        self.with_handlers(jid, |entry| {
            entry.text_chunk = Some(Arc::from(handler));
        });
    }

    fn on_todos_update(&self, jid: &str, handler: Box<dyn Fn(Vec<TodosUpdateItem>) + Send + Sync>) {
        self.with_handlers(jid, |entry| {
            entry.todos_update = Some(Arc::from(handler));
        });
    }

    fn on_compact_start(&self, jid: &str, handler: Box<dyn Fn(CompactStartData) + Send + Sync>) {
        self.with_handlers(jid, |entry| {
            entry.compact_start = Some(Arc::from(handler));
        });
    }

    fn on_compact_exec(&self, jid: &str, handler: Box<dyn Fn(CompactExecData) + Send + Sync>) {
        self.with_handlers(jid, |entry| {
            entry.compact_exec = Some(Arc::from(handler));
        });
    }

    fn on_session_error(&self, jid: &str, handler: Box<dyn Fn(SessionErrorData) + Send + Sync>) {
        self.with_handlers(jid, |entry| {
            entry.session_error = Some(Arc::from(handler));
        });
    }

    fn on_tool_permission_request(
        &self,
        jid: &str,
        handler: Box<dyn Fn(ToolPermissionRequestData) + Send + Sync>,
    ) {
        self.with_handlers(jid, |entry| {
            entry.tool_permission_request = Some(Arc::from(handler));
        });
    }

    fn on_ask_question_request(
        &self,
        jid: &str,
        handler: Box<dyn Fn(AskQuestionRequestData) + Send + Sync>,
    ) {
        self.with_handlers(jid, |entry| {
            entry.ask_question_request = Some(Arc::from(handler));
        });
    }

    fn on_form_request(
        &self,
        jid: &str,
        handler: Box<dyn Fn(crate::zen_core::FormRequestData) + Send + Sync>,
    ) {
        self.with_handlers(jid, |entry| {
            entry.form_request = Some(Arc::from(handler));
        });
    }

    fn on_conversation_usage(
        &self,
        jid: &str,
        handler: Box<dyn Fn(crate::zen_core::ConversationUsageData) + Send + Sync>,
    ) {
        self.with_handlers(jid, |entry| {
            entry.conversation_usage = Some(Arc::from(handler));
        });
    }

    fn add_allowed_tool(&self, jid: &str, tool: &str) {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.add_allowed_tool(tool);
        }
    }

    fn respond_to_tool_permission(&self, jid: &str, tool_name: &str, selected: &str) -> Result<()> {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.respond_to_tool_permission(ToolPermissionResponseData {
                tool_name: tool_name.to_string(),
                selected: selected.to_string(),
            });
        }
        Ok(())
    }

    fn respond_to_ask_question(
        &self,
        jid: &str,
        _agent_id: &str,
        answers: HashMap<String, String>,
    ) -> Result<()> {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.respond_to_ask_question(AskQuestionResponseData {
                agent_id: _agent_id.to_string(),
                answers,
            });
        }
        Ok(())
    }

    fn respond_to_form(
        &self,
        jid: &str,
        agent_id: &str,
        values: HashMap<String, serde_json::Value>,
        submitted: bool,
    ) -> Result<()> {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.respond_to_form(FormResponseData {
                agent_id: agent_id.to_string(),
                values,
                submitted,
            });
        }
        Ok(())
    }

    fn respond_to_plan_exit(&self, jid: &str, agent_id: &str, selected: &str) -> Result<()> {
        if let Some(engine) = self.engines.lock().unwrap().get(jid) {
            engine.respond_to_plan_exit(PlanExitResponseData {
                agent_id: agent_id.to_string(),
                selected: selected.to_string(),
            });
        }
        Ok(())
    }

    fn off_all(&self, jid: &str) {
        self.with_handlers(jid, |entry| {
            *entry = CoreHandlers::default();
        });
    }
}

/// A guaranteed-valid working directory to fall back to when no per-jid dir has
/// been seeded. The user's home is the least surprising place for an agent to
/// operate; if it is somehow unavailable, use the process cwd, then `/`. Never
/// returns an empty string — that is exactly the value that breaks Bash spawns.
fn default_working_dir() -> String {
    if let Some(home) = dirs::home_dir() {
        return home.to_string_lossy().to_string();
    }
    if let Ok(cwd) = std::env::current_dir() {
        return cwd.to_string_lossy().to_string();
    }
    "/".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback must never be empty (an empty `current_dir` is exactly what
    /// breaks Bash spawns) and must point at a directory that actually exists.
    #[test]
    fn default_working_dir_is_non_empty_and_exists() {
        let dir = default_working_dir();
        assert!(!dir.is_empty(), "default working dir must not be empty");
        assert!(
            std::path::Path::new(&dir).is_dir(),
            "default working dir must exist: {dir}"
        );
    }
}

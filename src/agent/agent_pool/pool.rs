//! AgentPool — core agent lifecycle management and dispatch coordination.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::Result;
use uuid::Uuid;

use crate::types::AgentApi;

use super::state::State;
use super::traits::{AgentEventSink, CachedTools, CoreApi};
use super::types::{
    AskQuestionRequestData, CachedTodos, CompactExecData, CompactStartData, MessageCompleteData,
    ProcessEvent, ReplyFn, SendReplyFn, SessionErrorData, StateUpdateData, TodoSnapshot,
    TodosUpdateItem, ToolPermissionRequestData, TypingFn, AGENT_TIMEOUT_MS, MAIN_AGENT_ID,
};
use super::workspace::WorkspaceStateFile;
use crate::agent::dispatch_bridge::{
    build_dispatch_resume_hint, AdminActivityCallback, DispatchBridgeApi,
};
use crate::agent::group_queue::GroupQueue;
use crate::agent::permission_bridge::{
    AskQuestionPayload, FormPayload, PermissionBridge, PermissionPayload,
};
use crate::agent::session_bridge;
use crate::config::Config;
use crate::db::Db;
use crate::mcp::helper::{
    background_mcp_config, browser_mcp_config, dispatch_mcp_config, js_mcp_config,
    litho_mcp_config, memory_mcp_config, ocr_mcp_config, schedule_mcp_config, send_mcp_config,
    docs_mcp_config, space_mcp_config, usage_mcp_config, wiki_mcp_config,
    workspace_mcp_config, McpServerConfig,
};
use crate::memory::daily_logger::DailyLogger;
use crate::types::GroupBinding;
use crate::types::MessageAttachment;
use crate::util::local_time::local_iso_string_now;

/// For Web UI only: fold structured `reasoning` + `content` into one string so the client
/// renders a single bubble (`extractLeadingReasoningBlocks` + collapsible). Non-`web:` JIDs
/// still receive plain `content` via [`AgentPool::broadcast_reply_now`].
fn merge_assistant_reasoning_for_web_ui(
    reasoning: &str,
    content: &str,
    has_tool_calls: bool,
) -> String {
    let r = reasoning.trim();
    if r.is_empty() {
        return content.to_string();
    }
    let c = content.trim();

    // Empty visible body — two very different cases, distinguished by whether
    // the model also produced a tool call this turn:
    //
    // - **Intermediate turn** (`has_tool_calls = true`): the model is just
    //   thinking about which tool to use; there's no user-facing answer yet
    //   (the next turn will produce it). Wrap reasoning in `<think>` so the
    //   UI shows the collapsible thinking widget alone — body stays empty,
    //   and the tool-execution chip rendered after it tells the user "work
    //   in progress." Surfacing the raw reasoning as the body here would
    //   dump ~1000 chars of "I will now invoke X" thinking on the user
    //   between every tool turn — confusing.
    //
    // - **Final turn** (`has_tool_calls = false`) but body still empty: the
    //   model almost certainly left its answer locked inside an unclosed
    //   `<|channel>thought\n…` block (Gemma-4 quirk — sometimes transitions
    //   straight from thinking to answer without sending `<channel|>`). The
    //   stream parser's `emit_unclosed_channel` smart-split handles most
    //   cases upstream; for the rest, surfacing the reasoning as the body
    //   here means the user always sees *something* (collapsible affordance
    //   sacrificed in this rare edge case — preferable to a blank message).
    if c.is_empty() {
        if has_tool_calls {
            return format!(
                "{open}\n{r}\n{close}",
                open = concat!("<", "think", ">"),
                close = concat!("</", "think", ">"),
            );
        }
        return r.to_string();
    }

    // Skip wrapping ONLY when the content already opens with a recognised
    // leading reasoning block (matches the UI's `extractLeadingReasoningBlocks`
    // regex shape: `^<think>` / `^<redacted_reasoning>` / `^<redacted_thinking>`).
    // The previous loose check (`contains "<think"` anywhere in the first 4 KB)
    // mis-fired whenever visible content mentioned `<think` incidentally — most
    // notably for Gemma-4 where the parser extracts `<|channel>thought…<channel|>`
    // into the reasoning field, and a code example containing "<think>" in the
    // visible body could then suppress the wrap, leaving the thinking
    // invisible in the UI. Anchoring the check at the start matches what the
    // UI regex actually parses.
    let trimmed = c.trim_start();
    // `chars().take(N)` instead of byte slicing — `trimmed[..N]` panics when N
    // lands mid-codepoint (Vietnamese `đ`, etc.). 32 chars is plenty to detect
    // the longest prefix we test for (`<redacted_reasoning>` = 20 chars).
    let lower_head = trimmed
        .chars()
        .take(32)
        .collect::<String>()
        .to_ascii_lowercase();
    let already_wrapped = lower_head.starts_with(concat!("<", "think", ">"))
        || lower_head.starts_with(concat!("<", "think", " "))
        || lower_head.starts_with(concat!("<", "redacted_", "reasoning", ">"))
        || lower_head.starts_with(concat!("<", "redacted_", "thinking", ">"));
    if already_wrapped {
        return content.to_string();
    }
    format!(
        "{open}\n{r}\n{close}\n\n{c}",
        open = concat!("<", "think", ">"),
        close = concat!("</", "think", ">"),
    )
}

// ===== AgentPool =====

pub struct AgentPool {
    core_api: Arc<dyn CoreApi>,
    pub(crate) state: Mutex<State>,

    // Optional dependencies wired after construction so lib.rs's existing
    // `AgentPool::new(core_api)` call still compiles.
    on_reply: Mutex<Option<ReplyFn>>,
    send_reply: Mutex<Option<SendReplyFn>>,
    typing_fn: Mutex<Option<TypingFn>>,
    permission_bridge: Mutex<Option<Arc<PermissionBridge>>>,
    workbench_bridge: Mutex<Option<Arc<crate::agent::workbench_bridge::WorkbenchBridge>>>,
    daily_logger: Mutex<Option<Arc<DailyLogger>>>,
    agent_event_sink: Mutex<Option<Arc<dyn AgentEventSink>>>,
    dispatch_bridge: Mutex<Option<Arc<dyn DispatchBridgeApi>>>,
    group_queue: Mutex<Option<Arc<GroupQueue>>>,

    /// `~/.senclaw/` — workspace state files live here.
    senclaw_home: Mutex<PathBuf>,

    /// DB handle — used by resume_agent to rebuild prompts from history.
    db: Mutex<Option<Arc<Db>>>,

    /// Runtime config mirror used for get_or_create MCP server wiring.
    config: Mutex<Option<Arc<Config>>>,

    /// Marketplace manager for loading MCP servers from plugins.
    marketplace_manager: Mutex<Option<Arc<crate::marketplace::manager::MarketplaceManager>>>,

    /// Runtime manager — engines (decision, OCR, …) reached over loopback HTTP
    /// instead of in-process. `None` in bare test setups.
    runtime_manager: Mutex<Option<Arc<crate::runtime::manager::RuntimeManager>>>,

    /// Weak self pointer so `&self` paths can upgrade to `Arc<Self>`
    /// when wiring long-lived callback closures (e.g. bind_events).
    self_weak: Mutex<Weak<AgentPool>>,
}

impl AgentPool {
    pub fn new(core_api: Arc<dyn CoreApi>) -> Arc<Self> {
        let default_home = dirs::home_dir()
            .map(|h| h.join(".senclaw"))
            .unwrap_or_else(|| PathBuf::from(".senclaw"));
        let pool = Arc::new(Self {
            core_api,
            state: Mutex::new(State::new()),
            on_reply: Mutex::new(None),
            send_reply: Mutex::new(None),
            typing_fn: Mutex::new(None),
            permission_bridge: Mutex::new(None),
            workbench_bridge: Mutex::new(None),
            daily_logger: Mutex::new(None),
            agent_event_sink: Mutex::new(None),
            dispatch_bridge: Mutex::new(None),
            group_queue: Mutex::new(None),
            senclaw_home: Mutex::new(default_home),
            db: Mutex::new(None),
            config: Mutex::new(None),
            marketplace_manager: Mutex::new(None),
            runtime_manager: Mutex::new(None),
            self_weak: Mutex::new(Weak::new()),
        });
        *pool.self_weak.lock().unwrap() = Arc::downgrade(&pool);
        pool
    }

    // ===== Dependency injection setters =====

    /// Web-UI reply callback — called by `broadcast_reply` for WS push.
    pub fn set_reply_callback(&self, f: ReplyFn) {
        *self.on_reply.lock().unwrap() = Some(f);
    }

    /// Channel send callback — wired by daemon so channel-bound replies bypass
    /// the WS-only reply path.
    pub fn set_send_reply(&self, f: SendReplyFn) {
        *self.send_reply.lock().unwrap() = Some(f);
    }

    pub fn set_typing_fn(&self, f: TypingFn) {
        *self.typing_fn.lock().unwrap() = Some(f);
    }

    fn send_typing(&self, jid: &str, active: bool, bot_token: Option<&str>) {
        if let Some(f) = self.typing_fn.lock().unwrap().as_ref().cloned() {
            f(jid, active, bot_token);
        }
    }

    pub fn set_permission_bridge(&self, bridge: Arc<PermissionBridge>) {
        // Reset inactivity timer while waiting on permission interactions.
        let weak = self.self_weak.lock().unwrap().clone();
        bridge.set_activity_callback(move |jid: &str| {
            if let Some(pool) = weak.upgrade() {
                pool.notify_activity(jid);
            }
        });

        // When user selects "allow" (never ask again), persist the tool to the
        // group's `approved_tools` list so future engines for this group start
        // with it pre-approved. NEVER write it into `allowed_tools`: that
        // column is the use_tools whitelist, and appending approvals there
        // shrinks the next session's whole tool roster to just the approved
        // tools (a single "always allow Skill" click used to strip every MCP
        // tool from a scheduled session).
        let weak2 = self.self_weak.lock().unwrap().clone();
        bridge.set_tool_allowed_callback(move |group_jid: &str, tool_name: &str| {
            let Some(pool) = weak2.upgrade() else { return };
            let group_jid = group_jid.to_string();
            let tool_name = tool_name.to_string();

            // Persist to DB (best-effort). Future engines re-read this column
            // at creation, so no in-memory binding update is needed.
            if let Some(db) = pool.db.lock().unwrap().as_ref() {
                if let Err(e) = db.append_group_approved_tool(&group_jid, &tool_name) {
                    tracing::warn!(
                        "[AgentPool] Failed to persist approved tool {tool_name} for {group_jid}: {e}"
                    );
                } else {
                    tracing::info!(
                        "[AgentPool] Persisted approved tool {tool_name} for group {group_jid}"
                    );
                }
            };
        });

        *self.permission_bridge.lock().unwrap() = Some(bridge);
    }

    pub fn set_daily_logger(&self, logger: Arc<DailyLogger>) {
        *self.daily_logger.lock().unwrap() = Some(logger);
    }

    /// Workbench bridge — bound to each per-group engine after creation.
    pub fn set_workbench_bridge(
        &self,
        bridge: Arc<crate::agent::workbench_bridge::WorkbenchBridge>,
    ) {
        *self.workbench_bridge.lock().unwrap() = Some(bridge);
    }

    /// Get the workbench bridge (if wired). Used by engine factory to bind.
    pub fn workbench_bridge(&self) -> Option<Arc<crate::agent::workbench_bridge::WorkbenchBridge>> {
        self.workbench_bridge.lock().unwrap().clone()
    }

    /// `~/.senclaw/` — overrides the home-dir default.
    pub fn set_senclaw_home(&self, dir: PathBuf) {
        *self.senclaw_home.lock().unwrap() = dir;
    }

    pub fn set_group_queue(&self, queue: Arc<GroupQueue>) {
        *self.group_queue.lock().unwrap() = Some(queue);
    }

    /// DB handle — used by resume_agent to rebuild prompts from history.
    pub fn set_db(&self, db: Arc<Db>) {
        *self.db.lock().unwrap() = Some(db);
    }

    /// Runtime config used by MCP registration in get_or_create.
    pub fn set_config(&self, cfg: Arc<Config>) {
        *self.config.lock().unwrap() = Some(Arc::clone(&cfg));
        self.core_api.set_runtime_config(cfg);
    }

    /// Marketplace manager for loading MCP servers from plugins.
    pub fn set_marketplace_manager(
        &self,
        manager: Arc<crate::marketplace::manager::MarketplaceManager>,
    ) {
        *self.marketplace_manager.lock().unwrap() = Some(manager);
    }

    /// Runtime manager — used by the pre-skill router and by OCR for
    /// text-only models, both of which now reach their engine over HTTP.
    pub fn set_runtime_manager(&self, manager: Arc<crate::runtime::manager::RuntimeManager>) {
        *self.runtime_manager.lock().unwrap() = Some(manager);
    }

    fn runtime_manager(&self) -> Option<Arc<crate::runtime::manager::RuntimeManager>> {
        self.runtime_manager.lock().unwrap().clone()
    }

    /// Inject the dispatch bridge and forward its admin-activity callback into
    /// `notify_activity`, mirroring TS `setDispatchBridge`.
    pub fn set_dispatch_bridge(self: &Arc<Self>, bridge: Arc<dyn DispatchBridgeApi>) {
        let weak = Arc::downgrade(self);
        let cb: AdminActivityCallback = Arc::new(move |admin_folder: &str| {
            let Some(pool) = weak.upgrade() else { return };
            let jid = {
                let s = pool.state.lock().unwrap();
                s.bindings
                    .iter()
                    .find(|(_, b)| b.folder == admin_folder)
                    .map(|(j, _)| j.clone())
            };
            if let Some(jid) = jid {
                pool.notify_activity(&jid);
            }
        });
        bridge.set_admin_activity_callback(cb);
        *self.dispatch_bridge.lock().unwrap() = Some(bridge);
    }

    /// Snapshot the currently-installed [`DispatchBridgeApi`] (if any).
    pub fn dispatch_bridge_snapshot(&self) -> Option<Arc<dyn DispatchBridgeApi>> {
        self.dispatch_bridge.lock().unwrap().clone()
    }

    /// Wire WsGateway sink + connect PermissionBridge callbacks to it.
    pub fn set_agent_event_sink(&self, sink: Arc<dyn AgentEventSink>) {
        if let Some(bridge) = self.permission_bridge.lock().unwrap().as_ref() {
            let s1 = Arc::clone(&sink);
            bridge.set_permission_request_callback(
                move |chat_jid: &str, req_id: &str, payload: PermissionPayload| {
                    s1.notify_permission_request(chat_jid, req_id, payload);
                },
            );
            let s2 = Arc::clone(&sink);
            bridge.set_ask_question_request_callback(
                move |chat_jid: &str, req_id: &str, payload: AskQuestionPayload| {
                    s2.notify_ask_question_request(chat_jid, req_id, payload);
                },
            );
            let s3 = Arc::clone(&sink);
            bridge.set_permission_resolved_callback(
                move |chat_jid: &str, req_id: &str, key: &str, label: &str| {
                    s3.notify_permission_resolved(chat_jid, req_id, key, label);
                },
            );
            let s4 = Arc::clone(&sink);
            bridge.set_ask_question_resolved_callback(
                move |chat_jid: &str, req_id: &str, answers: HashMap<String, String>| {
                    s4.notify_ask_question_resolved(chat_jid, req_id, answers);
                },
            );
            let s5 = Arc::clone(&sink);
            bridge.set_form_request_callback(
                move |chat_jid: &str, req_id: &str, payload: FormPayload| {
                    s5.notify_form_request(chat_jid, req_id, payload);
                },
            );
            let s6 = Arc::clone(&sink);
            bridge.set_form_resolved_callback(
                move |chat_jid: &str, req_id: &str, values: HashMap<String, serde_json::Value>| {
                    s6.notify_form_resolved(chat_jid, req_id, values);
                },
            );
        }
        *self.agent_event_sink.lock().unwrap() = Some(sink);
    }

    // ===== Permission / Thinking config =====

    pub fn get_permissions_config(&self) -> super::types::PermissionsConfig {
        let s = self.state.lock().unwrap();
        super::types::PermissionsConfig {
            skip_main_agent_permissions: s.skip_main_agent_permissions,
            skip_all_agents_permissions: s.skip_all_agents_permissions,
        }
    }

    /// Permission policy for virtual workers (DAG sub-agents).
    ///
    /// Workers are NOT the main agent — so they MUST NOT inherit
    /// `skip_main_agent_permissions`, which by name applies only to the
    /// orchestrator. Previously this method OR'd both flags together, which
    /// meant a user who toggled "skip main agent permissions" on the
    /// orchestrator also silently disabled permission gates for every DAG
    /// worker spawned underneath — workers would write files, run shell
    /// commands, etc. with no prompt.
    ///
    /// Now: workers only skip permissions when the user explicitly toggled
    /// the global "skip ALL agents" master switch. The narrower
    /// "skip MAIN agent" flag stays scoped to the main agent.
    pub fn get_skip_perms_for_virtual(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.skip_all_agents_permissions
    }

    /// Hot-update permission flags across every active core.
    pub fn set_permissions_config(&self, opts: super::types::PermissionsConfig) {
        let updates: Vec<(String, bool)> = {
            let mut s = self.state.lock().unwrap();
            s.skip_main_agent_permissions = opts.skip_main_agent_permissions;
            s.skip_all_agents_permissions = opts.skip_all_agents_permissions;
            let skip = Self::compute_skip_perms(&opts);
            s.bindings
                .iter()
                .filter(|(jid, _)| s.cores.contains(*jid))
                .map(|(jid, _b)| (jid.clone(), skip))
                .collect()
        };
        // Engines created later read this; the loop below only reaches the
        // ones that already exist.
        self.core_api
            .set_default_skip_permissions(Self::compute_skip_perms(&opts));
        let n = updates.len();
        for (jid, skip) in &updates {
            self.core_api.update_skip_permissions(jid, *skip);
        }
        tracing::info!(
            "[AgentPool] Permissions updated (skipMain={}, skipAll={}), hot-updated {} agent(s)",
            opts.skip_main_agent_permissions,
            opts.skip_all_agents_permissions,
            n
        );
    }

    /// Hot-update Thinking switch on every active core.
    pub fn set_thinking_enabled(&self, enabled: bool) {
        let cores: Vec<String> = {
            let mut s = self.state.lock().unwrap();
            s.thinking_enabled = enabled;
            s.cores.iter().cloned().collect()
        };
        let n = cores.len();
        for jid in &cores {
            self.core_api.update_thinking(jid, enabled);
        }
        tracing::info!(
            "[AgentPool] Thinking mode {}, hot-updated {} agent(s)",
            if enabled { "enabled" } else { "disabled" },
            n
        );
    }

    pub fn get_thinking_enabled(&self) -> bool {
        self.state.lock().unwrap().thinking_enabled
    }

    /// Whether to skip human-in-the-loop permission prompts for an agent run.
    ///
    /// Every chat is a full-privilege agent now (no admin/non-admin split), so
    /// `skip_main_agent_permissions` applies uniformly — there is no longer a
    /// per-binding or dispatch-agent distinction to consider.
    pub(crate) fn compute_skip_perms(opts: &super::types::PermissionsConfig) -> bool {
        opts.skip_all_agents_permissions || opts.skip_main_agent_permissions
    }

    /// Compute effective skip-perms for one binding using current flags.
    /// Used by Phase 2 `get_or_create` and `set_dispatch_workspace`.
    #[allow(dead_code)]
    pub(crate) fn resolve_skip_perms(&self, _binding: &GroupBinding) -> bool {
        let s = self.state.lock().unwrap();
        let opts = super::types::PermissionsConfig {
            skip_main_agent_permissions: s.skip_main_agent_permissions,
            skip_all_agents_permissions: s.skip_all_agents_permissions,
        };
        Self::compute_skip_perms(&opts)
    }

    pub fn permission_bridge(&self) -> Option<Arc<PermissionBridge>> {
        self.permission_bridge.lock().unwrap().clone()
    }

    /// First responder wins. Returns `false` if no bridge or already consumed.
    pub fn resolve_permission(&self, request_id: &str, option_key: &str) -> bool {
        match self.permission_bridge.lock().unwrap().as_ref() {
            Some(b) => b.resolve_permission(request_id, option_key),
            None => false,
        }
    }

    /// Web UI batch-answer questions. Defers to PermissionBridge.
    pub fn resolve_ask_question_batch(
        &self,
        request_id: &str,
        answers: &serde_json::Value,
        other_texts: Option<&serde_json::Value>,
    ) -> bool {
        match self.permission_bridge.lock().unwrap().as_ref() {
            Some(b) => b.resolve_ask_question_batch(request_id, answers, other_texts),
            None => false,
        }
    }

    /// Web UI / app submits a FormUI form (`submitted = false` = user skipped).
    /// Defers to PermissionBridge.
    pub fn resolve_form(
        &self,
        request_id: &str,
        values: HashMap<String, serde_json::Value>,
        submitted: bool,
    ) -> bool {
        match self.permission_bridge.lock().unwrap().as_ref() {
            Some(b) => b.resolve_form(request_id, values, submitted),
            None => false,
        }
    }

    /// Surface a permission request from a virtual agent (no persistent core).
    /// Calls `PermissionBridge::handle_permission_request` with the virtual_jid so the
    /// request shows up in the admin Web UI.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_virtual_permission_request(
        &self,
        virtual_jid: &str,
        tool_name: &str,
        permission_key: &str,
        title: &str,
        content: &serde_json::Value,
        options: &HashMap<String, String>,
    ) {
        if let Some(bridge) = self.permission_bridge.lock().unwrap().as_ref() {
            bridge.handle_permission_request_gated(
                tool_name,
                permission_key,
                title,
                content,
                options,
                virtual_jid, // group_jid
                virtual_jid, // chat_jid (virtual: prefix → broadcast_to_admins in notify.rs)
                None,        // bot_token
            );
        } else {
            tracing::warn!(
                "[AgentPool] handle_virtual_permission_request: no PermissionBridge for {virtual_jid}/{tool_name}"
            );
        }
    }

    /// Forward a tool-permission response to the underlying core instance.
    /// Used by [`PermissionBridgeApi`] wiring in daemon startup.
    pub fn respond_to_tool_permission(&self, group_jid: &str, tool_name: &str, selected: &str) {
        if let Err(e) = self
            .core_api
            .respond_to_tool_permission(group_jid, tool_name, selected)
        {
            tracing::warn!(
                "[AgentPool] respond_to_tool_permission failed for {group_jid}/{tool_name}: {e}"
            );
        }
    }

    /// Forward an ask-question response map to the underlying core instance.
    /// Used by [`PermissionBridgeApi`] wiring in daemon startup.
    pub fn respond_to_ask_question(
        &self,
        group_jid: &str,
        agent_id: &str,
        answers: HashMap<String, String>,
    ) {
        if let Err(e) = self
            .core_api
            .respond_to_ask_question(group_jid, agent_id, answers)
        {
            tracing::warn!(
                "[AgentPool] respond_to_ask_question failed for {group_jid}/{agent_id}: {e}"
            );
        }
    }

    /// Forward a FormUI response to the underlying core instance.
    /// Used by [`PermissionBridgeApi`] wiring in daemon startup.
    pub fn respond_to_form(
        &self,
        group_jid: &str,
        agent_id: &str,
        values: HashMap<String, serde_json::Value>,
        submitted: bool,
    ) {
        if let Err(e) = self
            .core_api
            .respond_to_form(group_jid, agent_id, values, submitted)
        {
            tracing::warn!("[AgentPool] respond_to_form failed for {group_jid}/{agent_id}: {e}");
        }
    }

    /// Deliver a plan-exit decision to the suspended `ExitPlanMode` tool for
    /// `group_jid`. On approval the engine also flips back to Agent mode.
    pub fn resolve_plan_exit(&self, group_jid: &str, agent_id: &str, selected: &str) {
        if let Err(e) = self
            .core_api
            .respond_to_plan_exit(group_jid, agent_id, selected)
        {
            tracing::warn!("[AgentPool] resolve_plan_exit failed for {group_jid}/{agent_id}: {e}");
        }
    }

    // ===== Dispatch coordination =====

    /// Temporarily switch a subagent's working dir to the admin's during a
    /// dispatch task. If the subagent core does not exist yet, the override
    /// is recorded so [Phase 2 `get_or_create`] can apply it after creation.
    pub fn set_dispatch_workspace(&self, jid: &str, workspace_dir: &str) {
        if workspace_dir.is_empty() {
            return;
        }
        let (binding_opt, has_core) = {
            let mut s = self.state.lock().unwrap();
            s.dispatch_workspace_overrides
                .insert(jid.to_string(), workspace_dir.to_string());
            (s.bindings.get(jid).cloned(), s.cores.contains(jid))
        };
        if !has_core {
            return;
        }
        self.core_api.set_working_dir(jid, workspace_dir);
        if let Some(b) = binding_opt {
            let skip = self.resolve_skip_perms(&b);
            self.core_api.update_skip_permissions(jid, skip);
        }
        tracing::info!("[AgentPool] Dispatch workspace set for {jid}: {workspace_dir}");
    }

    /// Restore the subagent's own workdir after dispatch completes.
    pub fn revert_dispatch_workspace(&self, jid: &str) {
        let binding_opt = {
            let mut s = self.state.lock().unwrap();
            if !s.dispatch_workspace_overrides.contains_key(jid) {
                return;
            }
            s.dispatch_workspace_overrides.remove(jid);
            s.bindings.get(jid).cloned()
        };
        let Some(binding) = binding_opt else {
            return;
        };
        if !self.state.lock().unwrap().cores.contains(jid) {
            return;
        }
        let state_file = self.workspace_state_file(&binding.folder);
        match std::fs::read_to_string(&state_file) {
            Ok(raw) => match serde_json::from_str::<WorkspaceStateFile>(&raw) {
                Ok(state) if !state.current_dir.is_empty() => {
                    self.core_api.set_working_dir(jid, &state.current_dir);
                    tracing::info!(
                        "[AgentPool] Dispatch workspace reverted for {jid}: {}",
                        state.current_dir
                    );
                }
                _ => self.core_api.clear_working_dir(jid),
            },
            Err(_) => self.core_api.clear_working_dir(jid),
        }
    }

    pub fn mark_dispatch_executing(&self, jid: &str) {
        self.state
            .lock()
            .unwrap()
            .dispatch_executing
            .insert(jid.to_string());
    }

    pub fn clear_dispatch_executing(&self, jid: &str) {
        self.state.lock().unwrap().dispatch_executing.remove(jid);
    }

    pub fn set_current_dispatch_task_id(&self, jid: &str, task_id: &str) {
        self.state
            .lock()
            .unwrap()
            .dispatch_task_map
            .insert(jid.to_string(), task_id.to_string());
    }

    /// Completes the active dispatch task after PAW observes success (`ProcessEvent::Idle`).
    /// Skipped if `expected_task_id` no longer matches `dispatch_task_map` (superseded).
    pub fn notify_dispatch_if_pending(&self, jid: &str, expected_task_id: Option<&str>) {
        let (content_opt, task_id, current_eq) = {
            let s = self.state.lock().unwrap();
            let content_opt = s.last_dispatch_replies.get(jid).cloned();
            let current_task_id = s.dispatch_task_map.get(jid).cloned();
            if let (Some(exp), Some(cur)) = (expected_task_id, current_task_id.as_ref()) {
                if cur != exp {
                    return;
                }
            }
            let final_task_id = expected_task_id
                .map(String::from)
                .or_else(|| current_task_id.clone());
            let cur_eq = matches!(
                (final_task_id.as_ref(), current_task_id.as_ref()),
                (Some(a), Some(b)) if a == b
            );
            (content_opt, final_task_id, cur_eq)
        };
        let bridge = self.dispatch_bridge.lock().unwrap().clone();
        let mut clear_reply = false;
        match (task_id.as_deref(), bridge.as_ref()) {
            (Some(tid), Some(b)) => {
                let text = content_opt.clone().unwrap_or_default();
                b.notify_task_done(tid, &text);
                if current_eq {
                    self.state.lock().unwrap().dispatch_task_map.remove(jid);
                }
                self.clear_dispatch_executing(jid);
                clear_reply = true;
            }
            (None, Some(b)) => {
                if let Some(content) = content_opt.clone() {
                    b.notify_reply(jid, &content);
                    self.clear_dispatch_executing(jid);
                    clear_reply = true;
                }
            }
            _ => {}
        }
        if clear_reply {
            self.state.lock().unwrap().last_dispatch_replies.remove(jid);
        }
    }

    // ===== Cached todos =====

    /// Snapshot of all cached todos — used for initial push on WS subscribe.
    pub fn get_all_cached_todos(&self) -> HashMap<String, CachedTodos> {
        self.state.lock().unwrap().cached_todos.clone()
    }

    /// Remove one agent's cached todo list (Console dismiss) so subscribe
    /// snapshots stop replaying it. The persisted `agent_todos` DB row is
    /// deleted by the gateway handler that calls this.
    pub fn dismiss_cached_todos(&self, jid: &str) {
        self.state.lock().unwrap().cached_todos.remove(jid);
    }

    /// Snapshot of all cached agent tool rosters — used for initial push on
    /// WS subscribe so the Agent Console can render the tools each running
    /// agent can use.
    pub fn get_all_cached_tools(&self) -> HashMap<String, CachedTools> {
        self.state.lock().unwrap().cached_tools.clone()
    }

    /// Build & broadcast the current tool roster for `binding`.  Called after
    /// `create_session` so the Web UI sees an entry the moment an agent comes
    /// online — even before any user message is processed.
    pub fn publish_agent_tools(&self, binding: &GroupBinding) {
        let tools = self.core_api.get_tool_infos(&binding.jid);
        if tools.is_empty() {
            return;
        }
        {
            let mut s = self.state.lock().unwrap();
            s.cached_tools.insert(
                binding.jid.clone(),
                CachedTools {
                    agent_name: binding.name.clone(),
                    tools: tools.clone(),
                },
            );
        }
        if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
            sink.notify_agent_tools(&binding.jid, &binding.name, &tools);
        }
    }

    // ===== Reply / activity =====

    /// Unified output: send to channel (when not web-only) and to WS gateway.
    pub async fn broadcast_reply(&self, jid: &str, text: &str, bot_token: Option<&str>) {
        self.broadcast_reply_now(jid, text, bot_token, 0);
    }

    /// Synchronous reply fanout used by event callbacks. `tokens` is the
    /// assistant message's output-token cost (0 = unknown); forwarded to the WS
    /// sink so the chat UI can show per-message token usage. Channel sends
    /// ignore it.
    fn broadcast_reply_now(&self, jid: &str, text: &str, bot_token: Option<&str>, tokens: u32) {
        if !jid.starts_with("web:") {
            let send = self.send_reply.lock().unwrap().clone();
            if let Some(send) = send {
                send(jid, text, bot_token);
            }
        }
        // WS push: prefer the structured sink, fall back to legacy ReplyFn.
        let sink = self.agent_event_sink.lock().unwrap().clone();
        if let Some(sink) = sink {
            sink.notify_agent_reply(jid, text, tokens);
        } else {
            let cb = self.on_reply.lock().unwrap().clone();
            if let Some(cb) = cb {
                cb(jid, text);
            }
        }

        // Persist bot reply to conversation history.
        if let Some(db) = self.db.lock().unwrap().as_ref() {
            let msg = crate::types::StoredMessage {
                message_id: format!("bot:{}", Uuid::new_v4()),
                chat_jid: jid.to_string(),
                sender_jid: String::new(),
                sender_name: "assistant".to_string(),
                content: text.to_string(),
                timestamp: local_iso_string_now(),
                is_from_me: false,
                is_bot_reply: true,
                reply_to_id: None,
                media_type: None,
                attachments: None,
            };
            let limit = self
                .config
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.agent.max_messages_per_group)
                .unwrap_or(100);
            if let Err(e) = db.insert_group_message(&msg, limit) {
                tracing::warn!("[AgentPool] Failed to persist bot reply for {jid}: {e}");
            }
        }
    }

    /// Reset the inactivity timer for a JID. Phase 2 populates the underlying
    /// map; in Phase 1 this is a quiet no-op.
    pub fn notify_activity(&self, jid: &str) {
        let cb = {
            let s = self.state.lock().unwrap();
            s.active_timer_resets.get(jid).cloned()
        };
        if let Some(cb) = cb {
            cb();
        }
    }

    // ===== Workspace state file =====

    /// `~/.senclaw/workspace-state-{folder}.json` — mirrors TS path scheme
    /// (with the `senclaw` brand rename).
    pub(crate) fn workspace_state_file(&self, folder: &str) -> PathBuf {
        let home = self.senclaw_home.lock().unwrap().clone();
        home.join(format!("workspace-state-{folder}.json"))
    }

    /// Initialize the workspace state file with the default working dir
    /// (skipped when the file already exists). Mirrors TS `initWorkspaceState`.
    #[allow(dead_code)] // wired by Phase 2 get_or_create
    pub(crate) fn init_workspace_state(state_file: &Path, default_dir: &Path) {
        if state_file.exists() {
            return;
        }
        let parent = match state_file.parent() {
            Some(p) => p,
            None => return,
        };
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("[AgentPool] Could not create workspace state dir: {e}");
            return;
        }
        let body = WorkspaceStateFile {
            current_dir: default_dir.to_string_lossy().to_string(),
            updated_at: local_iso_string_now(),
        };
        match serde_json::to_string_pretty(&body) {
            Ok(json) => {
                if let Err(e) = std::fs::write(state_file, json) {
                    tracing::warn!("[AgentPool] Could not init workspace state file: {e}");
                }
            }
            Err(e) => tracing::warn!("[AgentPool] Could not serialize workspace state: {e}"),
        }
    }

    // ===== get_or_create (Phase 2) =====

    /// Get or create a core for `binding`, using `pending_creates` as a
    /// concurrency lock so concurrent callers for the same JID wait on the
    /// in-flight creation instead of duplicating (mirrors TS 454–466).
    async fn get_or_create(&self, binding: &GroupBinding) -> Result<()> {
        // Fast path: already exists.
        if self.state.lock().unwrap().cores.contains(&binding.jid) {
            return Ok(());
        }

        // If another task is creating this JID, poll until it finishes.
        loop {
            let pending = self
                .state
                .lock()
                .unwrap()
                .pending_creates
                .contains(&binding.jid);
            if !pending {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            if self.state.lock().unwrap().cores.contains(&binding.jid) {
                return Ok(());
            }
        }

        // Acquire creation lock.
        {
            let mut s = self.state.lock().unwrap();
            if s.cores.contains(&binding.jid) {
                return Ok(());
            }
            s.pending_creates.insert(binding.jid.clone());
        }

        let result = self.get_or_create_internal(binding).await;
        self.state
            .lock()
            .unwrap()
            .pending_creates
            .remove(&binding.jid);
        result
    }

    /// Full core-creation path (mirrors TS 468–684).
    ///
    /// Semantics:
    ///   1. Sync allowedWorkDirs from config.json
    ///   2. Resolve skipPerms + skill dirs + tool list
    ///   3. Create ZenEngine instance (on-demand in ensure_engine)
    ///   4. Bind events via bind_events (permissions, replies, state, todos)
    ///   5. Inject MCP servers via CoreApi
    ///   6. Init MemoryManager index via manager::init_agent
    ///   7. createSession via engine.create_session
    ///   8. Apply pending workspace + thinking flag
    ///
    /// On failure: clean up event listeners + dispose core.
    async fn get_or_create_internal(&self, binding: &GroupBinding) -> Result<()> {
        // Double-check after acquiring lock.
        if self.state.lock().unwrap().cores.contains(&binding.jid) {
            return Ok(());
        }

        let binding = binding.clone();

        // Sync allowedWorkDirs from config.json (config.json overrides DB).
        // Mirrors TS AgentPool.ts:479-482.
        //
        // `group_type == "code"` is a *code session*: a chat pinned to one
        // repository. The engine that created these was removed in 83e9720;
        // the code-session REST layer (plans/260912-0141-code-v2, phase 2)
        // creates them again as thin wrappers over an ordinary chat, so the
        // branches below are kept, not dead.
        let allowed_work_dirs = if binding.group_type == "code" {
            // Code chat sessions are explicitly bound to project workspace selected
            // in code_sessions.workspace; never override from global config.
            binding.allowed_work_dirs.clone()
        } else {
            let cfg_lock = self.config.lock().unwrap();
            match cfg_lock.as_ref() {
                Some(cfg) => {
                    match crate::gateway::group_manager::get_agent_allowed_work_dirs(
                        &cfg.paths.global_config_path,
                        &binding.folder,
                    ) {
                        // Not in config → use DB value.
                        None => binding.allowed_work_dirs.clone(),
                        // In config: null = switching disallowed, dirs = override.
                        Some(config_dirs) => config_dirs,
                    }
                }
                None => binding.allowed_work_dirs.clone(),
            }
        };

        let skip_perms = self.resolve_skip_perms(&binding);

        // Skills are loaded per-engine via ZenEngine::initialize_plugins()
        // which scans bundled, global-compat, global-sema, and clawhub-managed dirs.
        // Priority order: bundled < user (~/.claude/skills) < managed (clawhub) < workspace

        // TS uses a JSON-based MCP config file that needed clearing; Rust uses
        // SharedMcpRegistry (per-process subprocess spawn) — no file to clear.

        // Tool list resolution is handled by ZenEngine::new() which registers
        // all tools (static + TodoWrite + Skill + Task). EXCLUDED_TOOLS filtering
        // mirrors TS AgentPool.ts:514-522.

        // ZenEngine instance is created on-demand in ZenCoreApi::ensure_engine()
        // with all configuration (instanceId, agentDataDir, workingDir, agentMode,
        // useTools, skillsExtraDirs, skip*Permissions). Mirrors TS 524-538.

        // PermissionBridge handlers are wired in bind_events:
        // on_tool_permission_request → PermissionBridge.handle_permission_request_gated
        // on_ask_question_request → PermissionBridge.handle_ask_question_request

        // Init workspace state file (mirrors TS 569-572).
        let home = self.senclaw_home.lock().unwrap().clone();
        let workspace_dir = if binding.group_type == "code" {
            allowed_work_dirs
                .as_ref()
                .and_then(|dirs| dirs.first())
                // The Web UI's folder picker only captures the raw path string
                // the user typed (it never reads the filesystem), so a leading
                // `~` reaches us unexpanded. Resolve it here — the backend owns
                // path access.
                .map(|p| crate::util::paths::expand_tilde(p))
                .unwrap_or_else(|| {
                    home.parent()
                        .map(|p| p.join("senclaw").join("workspace").join(&binding.folder))
                        .unwrap_or_else(|| {
                            PathBuf::from("senclaw")
                                .join("workspace")
                                .join(&binding.folder)
                        })
                })
        } else {
            home.parent()
                .map(|p| p.join("senclaw").join("workspace").join(&binding.folder))
                .unwrap_or_else(|| {
                    PathBuf::from("senclaw")
                        .join("workspace")
                        .join(&binding.folder)
                })
        };
        let state_file = home.join(format!("workspace-state-{}.json", binding.folder));
        Self::init_workspace_state(&state_file, &workspace_dir);
        if binding.group_type == "code" {
            // Code chat sessions must always stick to session workspace, even if
            // an old workspace-state file already exists from a previous run.
            let body = WorkspaceStateFile {
                current_dir: workspace_dir.to_string_lossy().to_string(),
                updated_at: local_iso_string_now(),
            };
            if let Ok(json) = serde_json::to_string_pretty(&body) {
                let _ = std::fs::write(&state_file, json);
            }
        }

        // Seed the engine's working dir BEFORE it is lazily created in
        // `create_session`. `ensure_engine` reads this via the per-jid cache;
        // without it the engine keeps the empty `ZenCoreOptions` default and
        // every Bash `current_dir("")` spawn fails with ENOENT (the whole "code"
        // feature returns `{"error": true}` on a fresh chat). Honor the current
        // dir already recorded in the state file (the user may have `cd`'d
        // elsewhere in a prior run); fall back to the computed workspace default.
        let effective_dir = std::fs::read_to_string(&state_file)
            .ok()
            .and_then(|raw| serde_json::from_str::<WorkspaceStateFile>(&raw).ok())
            .map(|s| s.current_dir)
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| workspace_dir.to_string_lossy().to_string());
        self.core_api.set_working_dir(&binding.jid, &effective_dir);

        let custom_memory_dir: Option<String> = None;
        let memory_index_folder = binding.folder.clone();

        // Inject MCP servers (mirrors TS 546-624) through CoreApi abstraction.
        // Each registration is best-effort: on failure we keep agent creation alive.
        if let Some(cfg) = self.config.lock().unwrap().clone() {
            let state_file_s = state_file.to_string_lossy().to_string();
            let workspace_s = workspace_dir.to_string_lossy().to_string();
            let db_path_s = cfg.paths.db_path.to_string_lossy().to_string();
            let agents_dir_s = cfg.paths.agents_dir.to_string_lossy().to_string();
            let user_profile_s = cfg.paths.user_profile_path.to_string_lossy().to_string();
            let dispatch_state_s = cfg.paths.dispatch_state_path.to_string_lossy().to_string();
            let virtual_agents_dir_s = cfg.paths.virtual_agents_dir.to_string_lossy().to_string();

            let mut mcp_servers: Vec<McpServerConfig> = Vec::new();

            let openai_key = (!cfg.memory.openai_api_key.is_empty())
                .then_some(cfg.memory.openai_api_key.as_str());
            let openai_base = (!cfg.memory.openai_base_url.is_empty())
                .then_some(cfg.memory.openai_base_url.as_str());
            let litho_model = (!cfg.mcp.litho_model_efficient.is_empty())
                .then_some(cfg.mcp.litho_model_efficient.as_str());
            let wiki_dir_s = cfg.paths.wiki_dir.to_string_lossy().to_string();
            let browser_v2 = crate::browser_agent::settings::engine_at(&cfg.paths.global_config_path)
                == crate::browser_agent::settings::Engine::V2;

            if cfg.mcp.bundled {
                // One subprocess hosting every built-in server, instead of the
                // fourteen below. Same servers, same env, same tools — see
                // `crate::mcp::core_server`.
                mcp_servers.push(crate::mcp::helper::core_mcp_config(
                    crate::mcp::helper::CoreMcpParams {
                        db_path: &db_path_s,
                        group_folder: &binding.folder,
                        chat_jid: &binding.jid,
                        workspace_state_file: &state_file_s,
                        default_workspace: &workspace_s,
                        allowed_work_dirs: allowed_work_dirs.as_deref(),
                        agents_dir: &agents_dir_s,
                        memory_folder: &memory_index_folder,
                        embedding_provider: Some(cfg.memory.embedding_provider.as_str()),
                        openai_api_key: openai_key,
                        openai_base_url: openai_base,
                        custom_memory_dir: custom_memory_dir.as_deref(),
                        dispatch_state_path: &dispatch_state_s,
                        virtual_agents_dir: &virtual_agents_dir_s,
                        wiki_dir: &wiki_dir_s,
                        send_bridge_port: 18081,
                        bot_token: binding.bot_token.as_deref(),
                        ws_port: cfg.ws_port,
                        agent_id: &binding.jid,
                        ui_port: cfg.ui_server.port,
                        browser_v2,
                        litho_binary: cfg.mcp.litho_binary.as_str(),
                        litho_model_efficient: litho_model,
                        user_profile_path: &user_profile_s,
                        js_timeout_ms: 5_000,
                        js_memory_mb: 128,
                        servers: crate::mcp::helper::DEFAULT_CORE_SERVERS,
                    },
                ));
            } else {
                mcp_servers.push(schedule_mcp_config(
                    &db_path_s,
                    &binding.folder,
                    &binding.jid,
                ));
                // Background-tasks MCP — a chat can create/manage autonomous
                // background work. Guard 2 (no self-replication) holds by
                // construction: this is only injected into real chat sessions, and
                // the background runner builds its own tool + MCP set per task, so a
                // background run never receives this server unless a task explicitly
                // requests it (and background_create exposes no raw MCP injection).
                mcp_servers.push(background_mcp_config(
                    &db_path_s,
                    &binding.folder,
                    &binding.jid,
                ));
                // Usage MCP — read-only token/cost accounting so any chat can ask
                // "hôm nay tốn bao nhiêu" without opening the dashboard.
                mcp_servers.push(usage_mcp_config(&db_path_s));
                mcp_servers.push(workspace_mcp_config(
                    &state_file_s,
                    &workspace_s,
                    allowed_work_dirs.as_deref(),
                ));
                // Project docs MCP — writes markdown into the working
                // directory, so it reads the same workspace state file the
                // workspace server writes.
                mcp_servers.push(docs_mcp_config(&state_file_s, &workspace_s));
                mcp_servers.push(send_mcp_config(
                    18081,
                    &binding.jid,
                    binding.bot_token.as_deref(),
                    &db_path_s,
                ));
                // Dispatch MCP — register for every group, not just admin ones.
                // DAG mode is now a first-class workflow any chat can opt into via
                // the AgentModeSelector, so the dispatch tools must be available
                // wherever a user might switch to DAG. The engine's per-mode tool
                // filter (zen_core/engine.rs `is_dag` branch) is what actually
                // exposes dispatch tools to the model — they're stripped out of
                // Plan mode, and in Agent mode they're available but unused
                // unless the model explicitly calls them. State is per-folder, so
                // groups don't share dispatch state.
                mcp_servers.push(dispatch_mcp_config(
                    &dispatch_state_s,
                    &binding.folder,
                    &binding.jid,
                    Some(&virtual_agents_dir_s),
                ));

                mcp_servers.push(memory_mcp_config(
                    &db_path_s,
                    &memory_index_folder,
                    &agents_dir_s,
                    Some(cfg.memory.embedding_provider.as_str()),
                    openai_key,
                    openai_base,
                    custom_memory_dir.as_deref(),
                ));
                // Cognitive memory used to ship as the `senclaw-cognitive` MCP
                // server here. It's now built into the in-process tool list
                // (see `tools::all_tools` — CogAdd/CogSearch/CogRecall/CogForget/
                // CogStats). The MCP server binary is still available for
                // out-of-process callers; we just don't spawn it as a per-agent
                // subprocess any more.
                mcp_servers.push(wiki_mcp_config(&wiki_dir_s));
                mcp_servers.push(space_mcp_config(
                    &db_path_s,
                    &binding.folder,
                    &binding.jid,
                    cfg.ui_server.port,
                ));
                mcp_servers.push(litho_mcp_config(
                    cfg.mcp.litho_binary.as_str(),
                    openai_base,
                    openai_key,
                    litho_model,
                ));
                mcp_servers.push(browser_mcp_config(cfg.ws_port, cfg.ui_server.port, &binding.jid, browser_v2));
                mcp_servers.push(ocr_mcp_config(cfg.ui_server.port));
                // Sandboxed JS executor — no shared state, just default limits.
                mcp_servers.push(js_mcp_config(5_000, 128));
                // OS-sandbox executor (sbx_* tools) — Seatbelt/bubblewrap/docker
                // isolated shell + real python/node, shared engine DB.
                mcp_servers.push(crate::mcp::helper::sandbox_mcp_config());
            }

            // Marketplace plugin MCP servers are *not* spawned here. They are
            // registered once on the shared `McpManager`
            // (`marketplace::mcp_sync`), which is what puts their tools in every
            // agent's roster and lists them at `GET /api/mcp-servers`. Spawning
            // a copy per chat session would give one plugin N processes and two
            // places to look when a tool misbehaves.


            tracing::info!(
                "[AgentPool] Preparing {} MCP server(s) for {}: {}",
                mcp_servers.len(),
                binding.jid,
                mcp_servers
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            for server in &mcp_servers {
                if let Err(e) = self.core_api.add_or_update_mcp_server(&binding.jid, server) {
                    tracing::warn!(
                        "[AgentPool] MCP {} unavailable for {}: {e}",
                        server.name,
                        binding.folder
                    );
                }
            }
        }

        // Init memory index for this agent folder — mirrors TS 628-639.
        let _ = &custom_memory_dir;
        crate::memory::manager::get_instance()
            .init_agent(&memory_index_folder)
            .await;

        // Compute use_tools whitelist — mirrors TS AgentPool.ts:514-523.
        // Task is excluded for normal chat groups (sub-agent spawning is
        // not the right tool there). COWORK leads run in DAG mode (set
        // below): we DON'T impose a whitelist for them — DAG's own tool
        // filter (`tools_for_main_agent`'s `is_dag` branch) already strips
        // every mutating tool and surfaces the `Dispatch*` tools. Forcing a
        // whitelist here would strip those dispatch tools and leave the
        // lead with nothing to orchestrate with.
        let is_cowork = binding.group_type == "cowork";
        let use_tools: Vec<String> = if is_cowork {
            // No whitelist — DAG mode governs the roster (read-only + dispatch).
            Vec::new()
        } else {
            const EXCLUDED_TOOLS: &[&str] = &["Task"];
            match &binding.allowed_tools {
                Some(list) => list
                    .iter()
                    .filter(|t| !EXCLUDED_TOOLS.contains(&t.as_str()))
                    .cloned()
                    .collect(),
                None => Vec::new(), // empty = no filter (all tools)
            }
        };
        if !use_tools.is_empty() {
            self.core_api.set_use_tools(&binding.jid, use_tools);
        }

        // createSession mirrors TS 641-653. If runtime core is not wired, default no-op.
        self.core_api.create_session(&binding.jid)?;

        // Seed PermissionManager with tools that have already been approved for
        // this group, so the "never ask again" list survives daemon restarts:
        //  - `allowed_tools` (the configured whitelist) — being whitelisted
        //    implies approval;
        //  - `approved_tools` (persisted "always allow" choices) — read fresh
        //    from DB because it is not part of GroupBinding.
        if let Some(tools) = &binding.allowed_tools {
            for tool in tools {
                self.core_api.add_allowed_tool(&binding.jid, tool);
            }
        }
        let approved: Vec<String> = self
            .db
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|db| db.get_group_approved_tools(&binding.jid).ok())
            .unwrap_or_default();
        for tool in &approved {
            self.core_api.add_allowed_tool(&binding.jid, tool);
        }

        // Reload skills excluding disabled ones — mirrors TS 657.
        let disabled = crate::skills::disabled::read_disabled_skills();
        self.core_api.reload_skills(&disabled);

        // Register core + binding (mirrors TS 658-659).
        {
            let mut s = self.state.lock().unwrap();
            s.cores.insert(binding.jid.clone());
            s.bindings.insert(binding.jid.clone(), binding.clone());
        }

        // Bind persistent listeners after the core is registered.
        if let Some(pool) = self.self_weak.lock().unwrap().upgrade() {
            pool.bind_events(&binding);
        }

        // Apply pending dispatch workspace (mirrors TS 661-666).
        let pending_ws = self
            .state
            .lock()
            .unwrap()
            .dispatch_workspace_overrides
            .get(&binding.jid)
            .cloned();
        if let Some(ref ws) = pending_ws {
            self.core_api.set_working_dir(&binding.jid, ws);
            tracing::info!(
                "[AgentPool] Applied pending dispatch workspace for {}: {ws}",
                binding.jid
            );
        }

        // Apply thinking flag (mirrors TS 669).
        self.core_api
            .update_thinking(&binding.jid, self.state.lock().unwrap().thinking_enabled);

        // Apply pending agent mode (set via UI before engine existed).
        // COWORK leads always orchestrate via the DAG dispatch flow, so they
        // default to "Dag" mode unless the UI has explicitly pinned another
        // mode. Without this the lead runs in Agent mode, where a text-only
        // turn ends the run — so the lead stalls mid-todo before delegating
        // every member / synthesizing the final answer. DAG mode's
        // completion-nudge (see `completion_nudge_for`) keeps it going until
        // a real dispatch executes and results are reported.
        let pending_mode = self
            .state
            .lock()
            .unwrap()
            .pending_agent_modes
            .get(&binding.jid)
            .cloned()
            .or_else(|| {
                if is_cowork {
                    Some("Dag".to_string())
                } else {
                    None
                }
            });
        if let Some(mode) = &pending_mode {
            if mode != "Agent" {
                self.core_api.update_agent_mode(&binding.jid, mode);
                tracing::info!(
                    "[AgentPool] Applied agent mode for {}: {mode}{}",
                    binding.jid,
                    if is_cowork { " (cowork default)" } else { "" }
                );
            }
        }

        // Register native dispatch tools (replaces senclaw-dispatch MCP subprocess).
        if let Some(cfg) = self.config.lock().unwrap().clone() {
            let dispatch_config = std::sync::Arc::new(crate::tools::DispatchToolsConfig {
                state_path: cfg.paths.dispatch_state_path.clone(),
                admin_folder: binding.folder.clone(),
                agents_config_dir: Some(cfg.paths.virtual_agents_dir.to_string_lossy().to_string()),
            });
            self.core_api.register_tools(
                &binding.jid,
                vec![
                    std::sync::Arc::new(crate::tools::DispatchListAgentsTool::new(
                        dispatch_config.clone(),
                    )),
                    std::sync::Arc::new(crate::tools::DispatchCreateParentTool::new(
                        dispatch_config.clone(),
                    )),
                    std::sync::Arc::new(crate::tools::DispatchCreateParentAndRunTool::new(
                        dispatch_config.clone(),
                    )),
                    std::sync::Arc::new(crate::tools::DispatchTaskTool::new(
                        dispatch_config.clone(),
                    )),
                    std::sync::Arc::new(crate::tools::DispatchAllTasksTool::new(dispatch_config)),
                ],
            );
        }

        tracing::info!(
            "[AgentPool] Created agent for {} (folder: {}, skipPerms: {skip_perms})",
            binding.jid,
            binding.folder
        );

        // Push the tool roster so the Agent Console can render this agent
        // as soon as it comes online (mirrors TS `agent:tools` event).
        self.publish_agent_tools(&binding);
        Ok(())
    }

    /// Event-driven process-and-wait with 30‑min inactivity timeout, abort guard,
    /// and 5‑retry on transient errors.  Mirrors TS `processAndWait` (AgentPool.ts:690–828).
    ///
    /// Sets [`State::process_event_txs`] so [`bind_events`] persistent handlers forward
    /// `state:update` / `session:error` events here.  Calls `process_user_input`
    /// (non-blocking), then enters a `tokio::select!` loop with resetTimer pattern:
    ///
    /// | Event            | Action                                  |
    /// |-----------------|-----------------------------------------|
    /// | `Idle`          | cleanup, resolve                        |
    /// | `Error(data)`   | classify → transient retry / network / fatal |
    /// | `Reset`         | restart inactivity timer                |
    /// | `Abort`         | cleanup, reject                         |
    /// | `Timeout`       | destroy, notify dispatch, reject        |
    pub(crate) async fn process_and_wait_inner(
        &self,
        jid: &str,
        group: &GroupBinding,
        prompt: &str,
        retries_left: u32,
    ) -> Result<()> {
        self.process_and_wait_inner_with_attachments(jid, group, prompt, &[], retries_left)
            .await
    }

    /// Directory that relative `@file` mentions resolve against: the session's
    /// live cwd if the agent has `cd`-ed, else the workspace it was bound to.
    /// `None` for chats with no workspace at all — relative mentions are then
    /// refused rather than guessed at.
    fn effective_work_dir(&self, jid: &str, group: &GroupBinding) -> Option<std::path::PathBuf> {
        if let Some(dir) = self.state.lock().unwrap().runtime_work_dirs.get(jid) {
            return Some(crate::util::paths::expand_tilde(dir));
        }
        group
            .allowed_work_dirs
            .as_ref()
            .and_then(|dirs| dirs.first())
            .map(|p| crate::util::paths::expand_tilde(p))
    }

    /// Whether the model this group's turn will run on accepts image blocks.
    ///
    /// Asks the engine's own resolver with the group's `llm_config_id`, the
    /// same override it pushes via `set_model_override`, so a chat pinned to a
    /// text-only model doesn't inherit the answer for the globally-active
    /// vision model (and vice versa). No config loaded yet → `false`, which
    /// routes to OCR: degrading beats a provider 400.
    fn model_supports_vision(&self, group: &GroupBinding) -> bool {
        let config_path = self
            .config
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.paths.global_config_path.clone());
        let Some(config_path) = config_path else {
            tracing::warn!("[AgentPool] no runtime config — treating model as vision-less");
            return false;
        };
        crate::zen_core::engine::ZenEngine::model_accepts_images(
            &config_path,
            group.llm_config_id.as_deref(),
        )
    }

    /// Transcribe attached images for a model that can't see them.
    ///
    /// Never fails the turn: a decode error, a missing OCR model or an engine
    /// error all become `text: None` for that image, which
    /// [`append_ocr_context`](crate::agent::input_builder::append_ocr_context)
    /// renders as an explicit "OCR unavailable" the model must own up to.
    async fn ocr_images(
        &self,
        images: &[crate::zen_core::ImageSource],
    ) -> Vec<crate::agent::input_builder::OcrExtract> {
        use base64::{engine::general_purpose::STANDARD, Engine as _};

        let runtime = self.runtime_manager();
        let mut out = Vec::with_capacity(images.len());
        for img in images {
            let ext = img.media_type.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("png");
            let filename = format!("image.{ext}");
            let text = match (runtime.as_ref(), STANDARD.decode(img.data.as_bytes())) {
                (Some(rt), Ok(bytes)) => {
                    match crate::runtime::clients::ocr_recognize(rt, bytes, &filename).await {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!("[AgentPool] OCR failed for an attachment: {e}");
                            None
                        }
                    }
                }
                (None, _) => None,
                (_, Err(e)) => {
                    tracing::warn!("[AgentPool] attachment is not valid base64: {e}");
                    None
                }
            };
            out.push(crate::agent::input_builder::OcrExtract {
                media_type: img.media_type.clone(),
                text,
            });
        }
        out
    }

    /// Save and read every non-image attachment on this turn.
    ///
    /// Runs on a blocking thread: decoding base64, writing to disk and unzipping
    /// a `.docx` are all synchronous work that would otherwise stall the runtime
    /// while the user waits on their own message.
    async fn read_documents(
        &self,
        jid: &str,
        docs: &[&MessageAttachment],
    ) -> Vec<crate::agent::documents::DocumentExtract> {
        let uploads_dir = self
            .config
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.paths.uploads_dir.clone());
        let Some(uploads_dir) = uploads_dir else {
            tracing::warn!("[AgentPool] no runtime config — cannot save attached documents");
            return Vec::new();
        };
        // One stamp for the whole turn: two files attached together sort next to
        // each other in the uploads directory.
        let stamp = crate::util::local_time::local_iso_string_now().replace(':', "-");
        let jid = jid.to_string();
        let owned: Vec<MessageAttachment> = docs.iter().map(|a| (*a).clone()).collect();

        tokio::task::spawn_blocking(move || {
            owned
                .iter()
                .map(|a| crate::agent::documents::process_document(&uploads_dir, &jid, &stamp, a))
                .collect()
        })
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("[AgentPool] document processing panicked: {e}");
            Vec::new()
        })
    }

    /// Turn a prompt plus its attachments into what the engine takes: final
    /// prompt text, and the image blocks to ride alongside it.
    ///
    /// Documents are read into the prompt first, so that
    /// [`build_agent_input`](crate::agent::input_builder::build_agent_input)
    /// then sees the complete text and can pick up image URLs / `@paths`
    /// written *inside* an attached document's extract as well as the explicit
    /// image attachments.
    pub(crate) async fn prepare_turn_input(
        &self,
        jid: &str,
        prompt: &str,
        attachments: &[MessageAttachment],
    ) -> (String, Vec<crate::zen_core::ImageSource>) {
        let (image_atts, doc_atts): (Vec<_>, Vec<_>) =
            attachments.iter().partition(|a| a.is_image());

        let prompt = if doc_atts.is_empty() {
            prompt.to_string()
        } else {
            let docs = self.read_documents(jid, &doc_atts).await;
            crate::agent::documents::append_document_context(prompt, &docs)
        };

        let build_result = if image_atts.is_empty() {
            crate::agent::input_builder::build_agent_input(&prompt, None)
        } else {
            let images: Vec<crate::agent::input_builder::ImageAttachment> = image_atts
                .iter()
                .map(|a| crate::agent::input_builder::ImageAttachment {
                    url: a.data_url.clone(),
                    mime_type: Some(a.mime_type.clone()),
                })
                .collect();
            crate::agent::input_builder::build_agent_input(&prompt, Some(&images))
        };

        if !build_result.image_srcs.is_empty() {
            tracing::info!(
                "[AgentPool] Detected {} image sources: {:?}",
                build_result.image_srcs.len(),
                build_result.image_srcs
            );
        }
        if !build_result.failures.is_empty() {
            tracing::warn!(
                "[AgentPool] Image load failures: {:?}",
                build_result.failures
            );
        }

        crate::agent::input_builder::split_input(build_result.input)
    }

    /// Hand the turn to the engine, routing any attached images.
    ///
    /// A text-only endpoint answers an image block with a hard 400, so the
    /// model's declared capability decides. Vision wins when available (it reads
    /// layout, charts and handwriting OCR drops); otherwise the images are
    /// transcribed and folded into the prompt text so the model at least knows
    /// what it was sent.
    pub(crate) async fn dispatch_user_input(
        &self,
        jid: &str,
        group: &GroupBinding,
        prompt: &str,
        images: Vec<crate::zen_core::ImageSource>,
    ) -> Result<()> {
        if images.is_empty() {
            return self.core_api.process_user_input(jid, prompt);
        }
        if self.model_supports_vision(group) {
            tracing::info!(
                "[AgentPool] jid={jid} sending {} image block(s) to a vision model",
                images.len()
            );
            return self
                .core_api
                .process_user_input_with_images(jid, prompt, images);
        }
        let extracts = self.ocr_images(&images).await;
        let recognized = extracts
            .iter()
            .filter(|e| e.text.as_deref().is_some_and(|t| !t.trim().is_empty()))
            .count();
        tracing::info!(
            "[AgentPool] jid={jid} model has no vision — OCR'd {} image(s), {recognized} with text",
            images.len()
        );
        let prompt_with_ocr = crate::agent::input_builder::append_ocr_context(prompt, &extracts);
        self.core_api.process_user_input(jid, &prompt_with_ocr)
    }

    /// The pre-skill router (Settings → Decision → pre-skill). Off: nothing.
    /// Shadow: route in the background and log it — the turn is never
    /// delayed and the legacy keyword matcher still decides. On: route before
    /// dispatch (bounded by `ROUTE_TIMEOUT`) and hand the engine the decision.
    async fn route_skill(&self, jid: &str, prompt: &str, pre_trigger: bool, config_path: &std::path::Path) {
        use crate::decision::settings::FeatureMode;
        let settings = crate::gateway::group_manager::load_decision_settings(config_path);
        let mode = settings.skills.mode;
        if mode == FeatureMode::Off {
            return;
        }
        // An explicit `#skill` / `/skill` directive is the engine's own and wins.
        let head = prompt.trim_start();
        if head.is_empty() || head.starts_with('#') || head.starts_with('/') {
            return;
        }
        let cards = self.core_api.skill_cards(jid);
        if cards.is_empty() {
            return;
        }
        let Some(runtime) = self.runtime_manager() else {
            return;
        };
        let db = self.db.lock().unwrap().clone();
        let (chat, text) = (jid.to_string(), prompt.to_string());
        let run = async move {
            let report = crate::decision::skill_route::route(&runtime, &text, &cards, pre_trigger).await;
            let label = if mode == FeatureMode::On { "on" } else { "shadow" };
            if let Some(db) = db {
                if let Err(e) = db.insert_skill_route_log(&chat, &text, label, pre_trigger, &report) {
                    tracing::warn!("[skill-route] could not log the decision: {e:#}");
                }
            }
            tracing::info!(
                "[skill-route] {label}: legacy={:?} route={:?} engine={:?} ({})",
                report.legacy.as_ref().map(|r| (&r.name, r.force)),
                report.route.as_ref().map(|r| (&r.name, r.force)),
                report.engine_pick,
                report.reason
            );
            report
        };
        if mode == FeatureMode::On {
            let report = run.await;
            self.core_api.set_skill_route(jid, report.route);
        } else {
            tokio::spawn(run);
        }
    }

    /// Shadow specs (§5 `input.guard.*`): never act on the answer, only
    /// trace it — controlled by `controlPlane.jevOff`/`shadow` (see
    /// `control_plane::ladder::should_ask`). Spawned off the turn's own task,
    /// same as `route_skill` in shadow mode, so a cold Laya load never delays
    /// a reply — this is pure observability, so there is nothing to await.
    async fn run_control_plane_shadow_specs(&self, jid: &str, prompt: &str, config_path: &std::path::Path) {
        use crate::control_plane::{ladder, registry::SpecRegistry, trace};
        let head = prompt.trim();
        if head.is_empty() {
            return;
        }
        let settings = crate::gateway::group_manager::load_control_plane_settings(config_path);
        if crate::control_plane::jev_off(&settings) {
            return;
        }
        let Some(runtime) = self.runtime_manager() else {
            return;
        };
        let (jid, text) = (jid.to_string(), head.to_string());
        tokio::spawn(async move {
            let reg = SpecRegistry::load();
            let state = crate::decision::json::Json::String(text.chars().take(2_000).collect());
            for spec_id in ["input.guard.override", "input.guard.scope", "input.guard.sensitive"] {
                let Some(spec) = reg.get(spec_id) else { continue };
                let Some(decision) = ladder::ask(&runtime, &settings, spec, state.clone()).await else {
                    continue;
                };
                if settings.record_decision_inputs {
                    trace::record_decision_input(
                        &settings,
                        &jid,
                        spec_id,
                        spec.version,
                        &state,
                        &ladder::build_request_questions(spec),
                    );
                }
                tracing::debug!(
                    "[control-plane] {spec_id}: answer={} confidence={:.2} band={} shadow={}",
                    decision.answer,
                    decision.confidence,
                    decision.band,
                    decision.shadow
                );
                trace::record_decision(&jid, decision);
            }
        });
    }

    /// Process-and-wait with attachment support (images and documents).
    pub(crate) async fn process_and_wait_inner_with_attachments(
        &self,
        jid: &str,
        group: &GroupBinding,
        prompt: &str,
        attachments: &[MessageAttachment],
        retries_left: u32,
    ) -> Result<()> {
        self.get_or_create(group).await?;
        // ---- Pre-process stage 1: pre-trigger-skill (global toggle) ----
        // Push the current `preTriggerSkill` flag to the engine before dispatch
        // so a confident keyword/trigger match force-loads the skill instead of
        // only hinting. Read per-turn so flips in the UI take effect immediately.
        let global_config_path = self
            .config
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.paths.global_config_path.clone());
        if let Some(ref path) = global_config_path {
            let enabled = crate::gateway::group_manager::get_pre_trigger_skill_enabled(path);
            self.core_api.set_pre_trigger_skill(jid, enabled);
            let after = crate::gateway::group_manager::get_after_process_enabled(path);
            self.core_api.set_after_process(jid, after);
            // Default-flow handlers (open link / media / search / note): render
            // the `## User defaults` block and push it into the engine so the
            // system prompt reflects the user's Plugins → Widget settings.
            // `None` when nothing is configured — the prompt stays unchanged.
            let defaults = crate::gateway::group_manager::get_defaults_config(path);
            self.core_api
                .set_user_defaults(jid, defaults.render_prompt_block());
            // ---- Pre-process stage 1b: the pre-skill router ----
            self.route_skill(jid, prompt, enabled, path).await;
            // ---- Control plane: shadow specs (never act, only trace) ----
            self.run_control_plane_shadow_specs(jid, prompt, path).await;
        }

        // ---- Per-group LLM override ----
        // Push the group's model selection to the engine before dispatch so the
        // reply uses the group's chosen LLM (falls back to the globally active
        // model when unset). Read per-turn so UI changes take effect immediately.
        self.core_api
            .set_model_override(jid, group.llm_config_id.clone());

        // ---- Pre-process stage 2: pre-retrieval injection ----
        // Three independent, user-toggleable backends:
        //   • MEMORY.md    → env-level `memory.pre_retrieval` (whole-file load)
        //   • Cognitive    → global `preCognitive` toggle (pre-cognitive stage)
        //   • Curated      → global `memoryRecall` toggle (hybrid FTS5/vector
        //                    search over curated memory/*.md, per-memory hits)
        // The memory backend now loads `~/.senclaw/agents/<folder>/MEMORY.md`
        // verbatim instead of FTS-searching chunks + daily history.
        let full_prompt = {
            let (do_memory, max_results) = {
                let cfg = self.config.lock().unwrap();
                let c = cfg.as_ref();
                (
                    c.map(|c| c.memory.pre_retrieval).unwrap_or(false),
                    c.map(|c| c.memory.search_max_results as usize).unwrap_or(5),
                )
            };
            let do_cognitive = global_config_path
                .as_deref()
                .map(crate::gateway::group_manager::get_pre_cognitive_enabled)
                .unwrap_or(false);
            let do_recall = global_config_path
                .as_deref()
                .map(crate::gateway::group_manager::get_memory_recall_enabled)
                .unwrap_or(false);

            // MEMORY.md backend — read the file verbatim from the agent's dir.
            let mem_context = if do_memory {
                let agents_dir = self
                    .config
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|c| c.paths.agents_dir.clone());
                match agents_dir {
                    Some(dir) => {
                        let memory_md = dir.join(&group.folder).join("MEMORY.md");
                        match std::fs::read_to_string(&memory_md) {
                            Ok(content) => content.trim().to_string(),
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                            Err(e) => {
                                tracing::warn!(
                                    "[AgentPool] Failed to read {}: {e}",
                                    memory_md.display()
                                );
                                String::new()
                            }
                        }
                    }
                    None => String::new(),
                }
            } else {
                String::new()
            };

            // Cognitive-graph backend (pre-cognitive stage).
            let cog_context = if do_cognitive {
                cognitive_pre_retrieval(prompt, &group.folder, max_results).await
            } else {
                String::new()
            };

            // Curated-memory backend (`memoryRecall` toggle).
            let recall_context = if do_recall {
                curated_pre_retrieval(prompt, &group.folder, jid, max_results).await
            } else {
                String::new()
            };

            let mut blocks = String::new();
            if !mem_context.is_empty() {
                blocks.push_str(&format!("<memory>\n{mem_context}\n</memory>\n"));
            }
            if !cog_context.is_empty() {
                blocks.push_str(&format!(
                    "<cognitive_memory>\n{cog_context}</cognitive_memory>\n"
                ));
            }
            if !recall_context.is_empty() {
                blocks.push_str(&format!(
                    "<memory_recall>\n{recall_context}\n</memory_recall>\n"
                ));
            }
            if blocks.is_empty() {
                prompt.to_string()
            } else {
                format!("{blocks}\n{prompt}")
            }
        };

        // Daily history log disabled — memory now flows only through
        // ~/.senclaw/agents/<folder>/MEMORY.md. Daily-log files are no
        // longer written or read.
        let _ = &self.daily_logger;

        // ---- P14 v2: auto-reflection (session window) ----
        // The user turn joins the group's reflection window; the window is
        // cognified as ONE call when it fills up or the chat goes idle —
        // see `agent_pool::reflection`. Non-blocking; failures stay inside
        // the flush task. The config flag lets ops disable it for privacy
        // or cost reasons.
        {
            let reflect_enabled = self
                .config
                .lock()
                .unwrap()
                .as_ref()
                .map(|c| c.memory.cognitive_reflection)
                .unwrap_or(true);
            if reflect_enabled {
                super::reflection::reflect_push(&group.folder, "User", prompt);
            }
        }

        // ---- event bridge channels ----
        // mpsc: bind_events persistent handlers forward state:update / session:error here.
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<ProcessEvent>();
        // oneshot: destroy_inner signals abort to break the event loop.
        let (abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<String>();

        // ---- register abort ----
        {
            let mut s = self.state.lock().unwrap();
            let jid_abort = jid.to_string();
            s.active_aborts.insert(
                jid.to_string(),
                Box::new(move |reason: &str| {
                    tracing::warn!("[AgentPool] Abort for {jid_abort}: {reason}");
                    let _ = abort_tx.send(reason.to_string());
                }),
            );
        }

        // ---- register reset-timer callback (used by PermissionBridge / notify_activity) ----
        {
            let tx = event_tx.clone();
            let mut s = self.state.lock().unwrap();
            s.active_timer_resets.insert(
                jid.to_string(),
                Arc::new(move || {
                    let _ = tx.send(ProcessEvent::Reset);
                }),
            );
        }

        // ---- wire process event sender ----
        {
            let mut s = self.state.lock().unwrap();
            s.process_event_txs.insert(jid.to_string(), event_tx);
        }

        // ---- typing indicator ON ----
        self.send_typing(jid, true, group.bot_token.as_deref());

        // ---- Pre-process stage 3: composer directives ----
        // `/skill`, `#skill` and `@file` are typed in the composer; without this
        // pass they reach the LLM as prose. Gated on a cheap regex so ordinary
        // messages never pay for the skill scan.
        let full_prompt = if crate::agent::prompt_directives::has_directives(&full_prompt) {
            let known_skills = {
                let cfg = self.config.lock().unwrap();
                match cfg.as_ref() {
                    Some(c) => crate::skills::scan::load_all_local_skills(c)
                        .into_iter()
                        .map(|s| s.name)
                        .collect::<Vec<_>>(),
                    None => Vec::new(),
                }
            };
            // Names only: `PatternRegistry::list` reads every `system.md` to
            // build descriptions, which for the Fabric library is 255 file
            // reads on a path that runs for every message containing a slash.
            let known_patterns = {
                let cfg = self.config.lock().unwrap();
                match cfg.as_ref() {
                    Some(c) => {
                        let store = crate::patterns::PatternStore::new(&c.paths.patterns_dir);
                        crate::patterns::PatternRegistry::new(&store).names()
                    }
                    None => Vec::new(),
                }
            };
            let work_dir = self.effective_work_dir(jid, group);
            let expansion = crate::agent::prompt_directives::expand(
                &full_prompt,
                work_dir.as_deref(),
                &known_skills,
                &known_patterns,
            );
            if !expansion.skills.is_empty()
                || !expansion.files.is_empty()
                || !expansion.patterns.is_empty()
            {
                tracing::info!(
                    "[AgentPool] directives jid={} skills={:?} patterns={:?} files={:?}",
                    jid,
                    expansion.skills,
                    expansion.patterns,
                    expansion.files
                );
            }
            for warning in &expansion.warnings {
                tracing::warn!("[AgentPool] directive warning jid={}: {}", jid, warning);
            }
            expansion.prompt
        } else {
            full_prompt
        };

        // ---- process user input with InputBuilder (attachment handling) ----
        // Mirrors TS AgentPool.ts:826: core.processUserInput(fullPrompt).
        // Documents are read into the prompt first, then InputBuilder resolves
        // images (explicit attachments + any URLs/@paths in the text itself).
        tracing::info!(
            "[AgentPool] process_user_input start jid={} prompt_len={} attachments={}",
            jid,
            full_prompt.len(),
            attachments.len()
        );

        let (processed_prompt, images) = self
            .prepare_turn_input(jid, &full_prompt, attachments)
            .await;

        self.dispatch_user_input(jid, group, &processed_prompt, images)
            .await?;

        let bot_token = group.bot_token.clone();
        let jid_owned = jid.to_string();
        let dispatch_task_id = {
            let s = self.state.lock().unwrap();
            s.dispatch_task_map.get(jid).cloned()
        };
        tracing::info!(
            "[AgentPool] PAW wait start jid={jid_owned} dispatch_task={} retries_left={retries_left}",
            dispatch_task_id.as_deref().unwrap_or("-")
        );

        // ---- event loop with resetTimer ----
        #[derive(Debug)]
        enum LoopResult {
            Success,
            Aborted(String),
            Error(SessionErrorData),
        }

        // Initial inactivity timer.
        let timeout_dur = Duration::from_millis(AGENT_TIMEOUT_MS);
        let mut timeout_fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            Box::pin(tokio::time::sleep(timeout_dur));

        let loop_result = loop {
            tokio::select! {
                biased;

                // Abort wins over everything (mirrors TS activeAborts callback).
                Ok(reason) = &mut abort_rx => {
                    tracing::warn!("[AgentPool] PAW aborted for {jid_owned}: {reason}");
                    break LoopResult::Aborted(reason);
                }

                // Event forwarded from bind_events persistent handlers.
                event = event_rx.recv() => {
                    match event {
                        Some(ProcessEvent::Idle) => {
                            let tid = self
                                .state
                                .lock()
                                .unwrap()
                                .dispatch_task_map
                                .get(&jid_owned)
                                .cloned()
                                .unwrap_or_else(|| "-".into());
                            tracing::info!("[AgentPool] PAW idle jid={jid_owned} dispatch_task={tid}");
                            break LoopResult::Success;
                        }
                        Some(ProcessEvent::Error(data)) => {
                            let tid = self
                                .state
                                .lock()
                                .unwrap()
                                .dispatch_task_map
                                .get(&jid_owned)
                                .cloned()
                                .unwrap_or_else(|| "-".into());
                            tracing::warn!(
                                "[AgentPool] PAW session error jid={jid_owned} dispatch_task={tid} code={} message={}",
                                data.code,
                                data.message
                            );
                            break LoopResult::Error(data);
                        }
                        Some(ProcessEvent::Reset) => {
                            // Restart inactivity timer (mirrors TS resetTimer).
                            let tid = self
                                .state
                                .lock()
                                .unwrap()
                                .dispatch_task_map
                                .get(&jid_owned)
                                .cloned()
                                .unwrap_or_else(|| "-".into());
                            tracing::info!(
                                "[AgentPool] PAW activity reset jid={jid_owned} dispatch_task={tid}"
                            );
                            timeout_fut = Box::pin(tokio::time::sleep(timeout_dur));
                        }
                        None => {
                            // Channel closed unexpectedly — treat as fatal error.
                            break LoopResult::Error(SessionErrorData {
                                code: "CHANNEL_CLOSED".into(),
                                message: "Event channel closed unexpectedly".into(),
                            });
                        }
                    }
                }

                // Inactivity timeout (30 min default).
                _ = &mut timeout_fut => {
                    tracing::warn!(
                        "[AgentPool] PAW timeout for {jid_owned} after {}ms",
                        AGENT_TIMEOUT_MS
                    );
                    self.destroy_inner(&jid_owned).await;
                    if let Some(bridge) = self.dispatch_bridge.lock().unwrap().as_ref() {
                        bridge.notify_error(&jid_owned, "Agent timeout");
                    }
                    if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                        sink.notify_agent_state(&jid_owned, "idle");
                    }
                    // Cleanup registrations.
                    {
                        let mut s = self.state.lock().unwrap();
                        s.active_timer_resets.remove(&jid_owned);
                        s.active_aborts.remove(&jid_owned);
                        s.process_event_txs.remove(&jid_owned);
                    }
                    return Err(anyhow::anyhow!("Agent timeout for {jid_owned}"));
                }
            }
        };

        // ---- cleanup registrations ----
        {
            let mut s = self.state.lock().unwrap();
            s.active_timer_resets.remove(&jid_owned);
            s.active_aborts.remove(&jid_owned);
            s.process_event_txs.remove(&jid_owned);
        }

        // ---- typing indicator OFF ----
        self.send_typing(&jid_owned, false, group.bot_token.as_deref());

        // ---- handle loop result ----
        match loop_result {
            LoopResult::Success => {
                if !self.state.lock().unwrap().cores.contains(&jid_owned) {
                    return Err(anyhow::anyhow!("Agent destroyed during processing"));
                }
                tracing::info!(
                    "[AgentPool] PAW success jid={jid_owned} dispatch_task={}",
                    dispatch_task_id.as_deref().unwrap_or("-")
                );
                if self
                    .state
                    .lock()
                    .unwrap()
                    .dispatch_executing
                    .contains(&jid_owned)
                {
                    // Complete dispatch only after PAW observes a clean idle transition.
                    // (The state:idle handler must not call notify_task_done: engine emits
                    // SessionError then Idle in order, and the idle callback ran before PAW
                    // could consume SessionError — incorrectly marking tasks done on LLM failure.)
                    self.notify_dispatch_if_pending(&jid_owned, dispatch_task_id.as_deref());
                }
                Ok(())
            }
            LoopResult::Aborted(reason) => {
                tracing::warn!(
                    "[AgentPool] PAW stopped jid={jid_owned} dispatch_task={} reason={reason}",
                    dispatch_task_id.as_deref().unwrap_or("-")
                );
                if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                    sink.notify_agent_state(&jid_owned, "idle");
                }
                Err(anyhow::anyhow!("Agent aborted"))
            }
            LoopResult::Error(data) => {
                let msg = format!("[{}] {}", data.code, data.message);
                let transient: &[&str] = &[
                    "terminated",
                    "Unexpected event order",
                    "API_RESPONSE_ERROR",
                    "API response format error",
                    "Premature close",
                    "missing finish_reason",
                    "no_available_session",
                    "No idle session profile",
                    "503 Service Unavailable",
                    "503",
                ];
                let is_transient = transient.iter().any(|p| msg.contains(p));
                let is_network = data.code == "NETWORK_ERROR" || msg.contains("NETWORK_ERROR");
                // EMPTY_COMPLETION is upstream-recoverable (auth blip / tool overload /
                // model not ready) — preserve the engine so the user can resend
                // without a 30s+ MCP cold-restart and history loss.
                let is_preservable =
                    data.code == "EMPTY_COMPLETION" || msg.contains("EMPTY_COMPLETION");

                if is_transient && retries_left > 0 {
                    tracing::warn!(
                        "[AgentPool] Transient error for {jid_owned}: {msg}, retrying in 3s ({retries_left} left)"
                    );
                    let was_dispatching = {
                        let mut s = self.state.lock().unwrap();
                        s.dispatch_executing.remove(&jid_owned)
                    };
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    if was_dispatching {
                        self.state
                            .lock()
                            .unwrap()
                            .dispatch_executing
                            .insert(jid_owned.clone());
                    }
                    Box::pin(self.process_and_wait_inner(
                        &jid_owned,
                        group,
                        prompt,
                        retries_left - 1,
                    ))
                    .await
                } else if is_network || is_preservable {
                    tracing::warn!(
                        "[AgentPool] {} error for {jid_owned}: {msg}, preserving session context",
                        if is_preservable && !is_network {
                            "Recoverable"
                        } else {
                            "Network"
                        }
                    );
                    self.core_api.interrupt_session(&jid_owned);
                    {
                        let mut s = self.state.lock().unwrap();
                        s.dispatch_executing.remove(&jid_owned);
                    }
                    if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                        sink.notify_agent_state(&jid_owned, "idle");
                    }
                    if let Some(bridge) = self.dispatch_bridge.lock().unwrap().as_ref() {
                        bridge.notify_error(&jid_owned, &format!("[{}] {msg}", data.code));
                    }
                    let user_msg = if is_preservable && !is_network {
                        format!(
                            "⚠️ The model returned no response (likely upstream issue: {msg}). \
                             Context preserved — just resend your message."
                        )
                    } else {
                        format!(
                            "⚠️ Network error: {msg}\nContext preserved — you can continue from where I left off."
                        )
                    };
                    self.broadcast_reply(&jid_owned, &user_msg, bot_token.as_deref())
                        .await;
                    Err(anyhow::anyhow!(msg))
                } else {
                    self.destroy_inner(&jid_owned).await;
                    if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                        sink.notify_agent_state(&jid_owned, "idle");
                    }
                    if let Some(bridge) = self.dispatch_bridge.lock().unwrap().as_ref() {
                        bridge.notify_error(
                            &jid_owned,
                            &format!("[{code}] {msg}", code = data.code, msg = data.message),
                        );
                    }
                    self.broadcast_reply(
                        &jid_owned,
                        &format!(
                            "❌ Session error [{code}]: {msg}\nSession has been reset.",
                            code = data.code,
                            msg = data.message
                        ),
                        bot_token.as_deref(),
                    )
                    .await;
                    Err(anyhow::anyhow!(msg))
                }
            }
        }
    }

    /// Internal destroy — aborts pending op, tears down core, cleans state.
    pub(crate) async fn destroy_inner(&self, jid: &str) {
        let dispatch_task = {
            let s = self.state.lock().unwrap();
            s.dispatch_task_map.get(jid).cloned()
        };
        tracing::warn!(
            "[AgentPool] destroy_inner jid={jid} dispatch_task={}",
            dispatch_task.as_deref().unwrap_or("-")
        );
        // Abort any pending process_and_wait.
        let abort = {
            let mut s = self.state.lock().unwrap();
            s.active_aborts.remove(jid)
        };
        if let Some(abort) = abort {
            abort("Agent destroyed");
        }

        self.core_api.destroy_agent(jid);
        let mut s = self.state.lock().unwrap();
        s.cores.remove(jid);
        s.bindings.remove(jid);
        s.cached_todos.remove(jid);
        s.dispatch_executing.remove(jid);
        s.dispatch_workspace_overrides.remove(jid);
        s.last_dispatch_replies.remove(jid);
        s.dispatch_task_map.remove(jid);
        s.runtime_work_dirs.remove(jid);
        s.active_timer_resets.remove(jid);
    }

    /// Whether a core has been created for this JID.
    pub fn has_agent(&self, jid: &str) -> bool {
        self.state.lock().unwrap().cores.contains(jid)
    }

    /// Active agent JIDs.
    pub fn active_jids(&self) -> Vec<String> {
        self.state.lock().unwrap().cores.iter().cloned().collect()
    }

    // ===== Phase 3: pause / resume / stop / destroy =====

    /// Pause the agent for `jid`. Three modes (mirrors TS 931–982):
    /// Switch the engine's agent mode for `jid`. Mode values: `"Agent"` (default)
    /// or `"Plan"`. Idempotent — unknown modes are ignored with a warning.
    /// The change applies to the next LLM turn: Plan mode strips
    /// `TodoWrite` from the tool list and injects the plan-mode reminder.
    pub fn set_agent_mode(&self, jid: &str, mode: &str) {
        // Always store in pending map so it survives engine destroy/recreate.
        self.state
            .lock()
            .unwrap()
            .pending_agent_modes
            .insert(jid.to_string(), mode.to_string());
        // If engine exists, apply immediately.
        self.core_api.update_agent_mode(jid, mode);
    }

    pub fn get_agent_mode(&self, jid: &str) -> Option<String> {
        // Try live engine first, fall back to pending.
        self.core_api.get_agent_mode(jid).or_else(|| {
            self.state
                .lock()
                .unwrap()
                .pending_agent_modes
                .get(jid)
                .cloned()
        })
    }

    ///   A. **core-pause** — active PAW → `CoreApi::pause_session`
    ///   B. **dispatch-pause** — active dispatch → record in set, notify
    ///   C. **synth-pause** — fully idle → record in set, notify
    ///
    /// If this agent is a dispatch admin, also pauses active subagents.
    pub fn pause_agent(&self, jid: &str) {
        let has_core = self.state.lock().unwrap().cores.contains(jid);
        if !has_core {
            tracing::warn!("[AgentPool] pause_agent: no active agent for {jid}");
            return;
        }

        let has_active_paw = { self.state.lock().unwrap().active_aborts.contains_key(jid) };
        let admin_folder = {
            self.state
                .lock()
                .unwrap()
                .bindings
                .get(jid)
                .map(|b| b.folder.clone())
        };
        let has_active_dispatch = admin_folder
            .as_ref()
            .and_then(|folder| {
                let bridge = self.dispatch_bridge.lock().unwrap().clone();
                bridge.map(|b| (b, folder.clone()))
            })
            .map(|(bridge, folder)| bridge.has_active_dispatch(&folder))
            .unwrap_or(false);

        let pause_mode: &str;
        if has_active_paw {
            self.core_api.pause_session(jid);
            pause_mode = "core-pause";
        } else if has_active_dispatch {
            {
                let mut s = self.state.lock().unwrap();
                s.dispatch_paused_jids.insert(jid.to_string());
            }
            if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                sink.notify_agent_state(jid, "paused");
            }
            pause_mode = "dispatch-pause";
        } else {
            {
                let mut s = self.state.lock().unwrap();
                s.synth_paused_jids.insert(jid.to_string());
            }
            if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                sink.notify_agent_state(jid, "paused");
            }
            pause_mode = "synth-pause";
        }

        // If admin: pause dispatch + active subagents.
        if let (Some(folder), Some(bridge)) = (
            admin_folder,
            self.dispatch_bridge.lock().unwrap().as_ref().cloned(),
        ) {
            let child_jids = bridge.pause_admin(&folder);
            let mut actually_paused: Vec<String> = Vec::new();
            for child_jid in &child_jids {
                let has_active = {
                    self.state
                        .lock()
                        .unwrap()
                        .active_aborts
                        .contains_key(child_jid.as_str())
                };
                if has_active {
                    self.core_api.pause_session(child_jid);
                    actually_paused.push(child_jid.clone());
                }
            }
            if !actually_paused.is_empty() {
                self.state
                    .lock()
                    .unwrap()
                    .paused_children_by_admin
                    .insert(jid.to_string(), actually_paused);
            }
        }

        tracing::info!("[AgentPool] Paused agent for {jid} ({pause_mode})");
    }

    /// Resume the agent for `jid`, optionally with a follow-up `query`.
    /// Mirrors TS 998–1077 — three scenarios matching pause_agent.
    pub fn resume_agent(self: &Arc<Self>, jid: &str, query: Option<&str>) {
        let has_core = self.state.lock().unwrap().cores.contains(jid);
        if !has_core {
            tracing::warn!("[AgentPool] resume_agent: no active agent for {jid}");
            return;
        }

        let was_synth_paused = { self.state.lock().unwrap().synth_paused_jids.contains(jid) };
        let was_dispatch_paused = {
            self.state
                .lock()
                .unwrap()
                .dispatch_paused_jids
                .contains(jid)
        };
        let was_idle_paused = was_synth_paused || was_dispatch_paused;
        let admin_folder = {
            self.state
                .lock()
                .unwrap()
                .bindings
                .get(jid)
                .map(|b| b.folder.clone())
        };

        if was_idle_paused {
            // Scenario B/C: core was idle — do not inject processUserInput
            // unless there is a query.
            {
                let mut s = self.state.lock().unwrap();
                s.synth_paused_jids.remove(jid);
                s.dispatch_paused_jids.remove(jid);
            }
            if let Some(q) = query.filter(|q| !q.trim().is_empty()) {
                if self.core_api.has_session_tool_results(jid) {
                    let prompt = format!(
                        "{q}\n\nBased on the work completed so far and the latest instruction above, decide how to continue."
                    );
                    let _ = self.core_api.process_user_input(jid, &prompt);
                } else {
                    // Rebuild prompt from DB history and run process_and_wait.
                    let binding = { self.state.lock().unwrap().bindings.get(jid).cloned() };
                    if let Some(binding) = binding {
                        let db = self.db.lock().unwrap().clone();
                        if let Some(db) = db {
                            let (db_prompt, _) = session_bridge::build_prompt_for_group(&db, jid);
                            if !db_prompt.is_empty() {
                                let pool = Arc::clone(self);
                                let jid = jid.to_string();
                                let binding2 = binding.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = AgentApi::process_and_wait(
                                        pool.as_ref(),
                                        &jid,
                                        &binding2,
                                        &db_prompt,
                                    )
                                    .await
                                    {
                                        tracing::error!(
                                            "[AgentPool] resume_agent process_and_wait error: {e}"
                                        );
                                    }
                                });
                            }
                        } else if !self.state.lock().unwrap().active_aborts.contains_key(jid) {
                            if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                                sink.notify_agent_state(jid, "idle");
                            }
                        }
                    }
                }
            } else if !self.state.lock().unwrap().active_aborts.contains_key(jid) {
                // Push idle only when no PAW race is confirmed.
                if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                    sink.notify_agent_state(jid, "idle");
                }
            }
            // active_aborts already exists → frontend updates via bindEvents.
        } else {
            // Scenario A: core was processing — continue with processUserInput.
            let base = query.unwrap_or("Go on.");
            let dispatch_ctx = admin_folder
                .as_ref()
                .and_then(|folder| {
                    self.dispatch_bridge.lock().unwrap().as_ref().map(|b| {
                        build_dispatch_resume_hint(Some(b.as_ref()), folder).unwrap_or_default()
                    })
                })
                .unwrap_or_default();
            let hint = if query.is_some()
                && !query.unwrap().trim().is_empty()
                && self.core_api.has_session_tool_results(jid)
            {
                "\n\nBased on the work completed so far and the latest instruction above, decide how to continue."
            } else {
                ""
            };
            let prompt = if dispatch_ctx.is_empty() {
                format!("{base}{hint}")
            } else {
                format!("{base}{hint}\n\n{dispatch_ctx}")
            };
            let _ = self.core_api.process_user_input(jid, &prompt);
        }

        // All scenarios: resume dispatch scheduling + paused subagents.
        if let (Some(folder), Some(bridge)) = (
            admin_folder,
            self.dispatch_bridge.lock().unwrap().as_ref().cloned(),
        ) {
            bridge.resume_admin(&folder);
            let paused_children = {
                self.state
                    .lock()
                    .unwrap()
                    .paused_children_by_admin
                    .remove(jid)
                    .unwrap_or_default()
            };
            for child_jid in &paused_children {
                let _ = self.core_api.process_user_input(child_jid, "Go on.");
            }
        }

        let resume_mode = if was_dispatch_paused {
            "dispatch-resume"
        } else if was_synth_paused {
            "synth-resume"
        } else {
            "core-resume"
        };
        tracing::info!("[AgentPool] Resumed agent for {jid} ({resume_mode})");
    }

    /// Terminate agent session for `jid`, discard all context, start fresh.
    /// Mirrors TS 1087–1147.
    pub async fn stop_agent(&self, jid: &str) {
        // 1. Notify dispatch if this agent is executing a subtask.
        if let Some(bridge) = self.dispatch_bridge.lock().unwrap().as_ref() {
            bridge.notify_error(jid, "Agent stopped by user");
        }
        {
            let mut s = self.state.lock().unwrap();
            s.dispatch_task_map.remove(jid);
            s.last_dispatch_replies.remove(jid);
        }

        // 2. Cancel admin dispatch parents + stop child subagents.
        let admin_folder = {
            self.state
                .lock()
                .unwrap()
                .bindings
                .get(jid)
                .map(|b| b.folder.clone())
        };
        let child_jids: Vec<String> = admin_folder
            .as_ref()
            .and_then(|folder| {
                self.dispatch_bridge
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|b| b.cancel_admin_parents(folder))
            })
            .unwrap_or_default();

        // 3. Clear backlog queue.
        let gq = { self.group_queue.lock().unwrap().clone() };
        if let Some(gq) = gq {
            gq.clear_queue(jid).await;
        }

        // 4. Abort pending process_and_wait.
        let abort = {
            let mut s = self.state.lock().unwrap();
            s.active_aborts.remove(jid)
        };
        if let Some(abort) = abort {
            abort("Agent stopped by user");
        }

        // 5. Recreate session.
        let has_core = self.state.lock().unwrap().cores.contains(jid);
        if has_core {
            match self.core_api.create_session(jid) {
                Ok(()) => {
                    // create_session rebuilds the skill registry without the
                    // disabled filter — re-apply it or disabled skills come
                    // back to life after stop/reset (upstream 8ef3dba).
                    let disabled = crate::skills::disabled::read_disabled_skills();
                    self.core_api.reload_skills(&disabled);
                    if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                        sink.notify_agent_state(jid, "idle");
                    }
                    tracing::info!("[AgentPool] Stopped and reset agent for {jid}");
                }
                Err(e) => {
                    tracing::error!("[AgentPool] stop_agent create_session failed for {jid}: {e}");
                    if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                        sink.notify_agent_state(jid, "idle");
                    }
                }
            }
        }

        // Clear residual paused states.
        {
            let mut s = self.state.lock().unwrap();
            s.synth_paused_jids.remove(jid);
            s.dispatch_paused_jids.remove(jid);
            s.paused_children_by_admin.remove(jid);
            s.last_dispatch_replies.remove(jid);
        }
        if self.state.lock().unwrap().cached_todos.contains_key(jid) {
            let name = {
                self.state
                    .lock()
                    .unwrap()
                    .bindings
                    .get(jid)
                    .map(|b| b.name.clone())
                    .unwrap_or_else(|| jid.to_string())
            };
            self.state.lock().unwrap().cached_todos.remove(jid);
            if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                sink.notify_agent_todos(jid, &name, &[]);
            }
        }

        // Recursively stop child subagents.
        for child_jid in &child_jids {
            Box::pin(self.stop_agent(child_jid)).await;
        }
    }

    /// Full cleanup — dispose core, unwatch files, clear dispatch state, notify
    /// frontend. Mirrors TS 1150–1217.
    pub async fn destroy_agent_full(&self, jid: &str) {
        // Interrupt pending PAW.
        let abort = {
            let mut s = self.state.lock().unwrap();
            s.active_aborts.remove(jid)
        };
        if let Some(abort) = abort {
            abort("Agent destroyed");
        }

        // Stop workspace file watcher.
        {
            let mut s = self.state.lock().unwrap();
            if let Some(unwatch) = s.workspace_watchers.remove(jid) {
                unwatch();
            }
        }

        // Notify frontend (state → idle) BEFORE removing event listeners.
        if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
            sink.notify_agent_state(jid, "idle");
        }

        // Remove persistent event listeners.
        {
            let mut s = self.state.lock().unwrap();
            if let Some(cleanup) = s.event_cleanups.remove(jid) {
                cleanup();
            }
        }

        // Stop memory file watch (mirrors TS AgentPool.ts:1178-1180).
        {
            let s = self.state.lock().unwrap();
            if let Some(binding) = s.bindings.get(jid) {
                let mgr = crate::memory::manager::get_instance();
                mgr.destroy_agent(&binding.folder);
            }
        }

        // Clean dispatch-related state.
        if let Some(bridge) = self.dispatch_bridge.lock().unwrap().as_ref() {
            bridge.notify_error(jid, "Agent destroyed");
        }
        {
            let mut s = self.state.lock().unwrap();
            s.last_dispatch_replies.remove(jid);
            s.dispatch_task_map.remove(jid);
            s.dispatch_executing.remove(jid);
            s.dispatch_workspace_overrides.remove(jid);
            s.synth_paused_jids.remove(jid);
            s.dispatch_paused_jids.remove(jid);
            s.runtime_work_dirs.remove(jid);
            s.bindings.remove(jid);
        }

        // Clear todos cache + notify.
        if self.state.lock().unwrap().cached_todos.contains_key(jid) {
            let name = {
                self.state
                    .lock()
                    .unwrap()
                    .bindings
                    .get(jid)
                    .map(|b| b.name.clone())
                    .unwrap_or_else(|| jid.to_string())
            };
            self.state.lock().unwrap().cached_todos.remove(jid);
            if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
                sink.notify_agent_todos(jid, &name, &[]);
            }
        }

        // Dispose core.
        let has_core = self.state.lock().unwrap().cores.contains(jid);
        if has_core {
            self.core_api.clear_working_dir(jid);
            self.core_api.destroy_agent(jid);
            self.state.lock().unwrap().cores.remove(jid);
        }

        // Final state push.
        if let Some(sink) = self.agent_event_sink.lock().unwrap().as_ref() {
            sink.notify_agent_state(jid, "idle");
        }

        // Clean remaining state maps.
        {
            let mut s = self.state.lock().unwrap();
            s.active_timer_resets.remove(jid);
        }
    }

    /// Destroy all agents (called on shutdown). Mirrors TS 1220–1224.
    pub async fn destroy_all(&self) {
        let jids = self.active_jids();
        tracing::info!("[AgentPool] Destroying {} agent(s)", jids.len());
        for jid in &jids {
            self.destroy_agent_full(jid).await;
        }
    }

    // ===== bind_events (Phase 4) =====

    /// Register persistent event listeners on the core and store cleanup.
    /// Mirrors TS `bindEvents` (AgentPool.ts:1299–1390).
    ///
    /// Event handlers forward to [`AgentEventSink`] and update internal state.
    pub fn bind_events(self: &Arc<Self>, binding: &GroupBinding) {
        let _jid = binding.jid.clone();
        let _folder = binding.folder.clone();
        let _name = binding.name.clone();
        let _bot_token = binding.bot_token.clone();

        // Mutable state shared across event handlers.
        let last_reply: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        // ---- message:complete ----
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let folder = _folder.clone();
            let bot_token = _bot_token.clone();
            let last_reply = Arc::clone(&last_reply);
            let jid_arg = jid.clone();
            self.core_api.on_message_complete(
                &jid_arg,
                Box::new(move |data: MessageCompleteData| {
                    if data.agent_id != MAIN_AGENT_ID {
                        return;
                    }
                    let dispatch_task = pool
                        .state
                        .lock()
                        .unwrap()
                        .dispatch_task_map
                        .get(&jid)
                        .cloned()
                        .unwrap_or_else(|| "-".into());
                    tracing::info!(
                        "[AgentPool] message_complete jid={jid} dispatch_task={dispatch_task} content_len={}",
                        data.content.len()
                    );
                    if data.content.trim().is_empty() && data.reasoning.trim().is_empty() {
                        tracing::info!(
                            "[AgentPool] message_complete empty content jid={jid} dispatch_task={dispatch_task}"
                        );
                        return;
                    }
                    *last_reply.lock().unwrap() = data.content.clone();
                    {
                        let mut s = pool.state.lock().unwrap();
                        s.last_dispatch_replies
                            .insert(jid.clone(), data.content.clone());
                    }
                    let reply_text = merge_assistant_reasoning_for_web_ui(
                        &data.reasoning,
                        &data.content,
                        data.has_tool_calls,
                    );
                    pool.broadcast_reply_now(
                        &jid,
                        &reply_text,
                        bot_token.as_deref(),
                        data.output_tokens,
                    );
                    // Assistant reply no longer appended to daily history log.
                    let _ = &pool.daily_logger;
                    // P14 v2: the assistant turn joins the same reflection
                    // window as the user turns, so cross-turn facts
                    // (question → answer) reach the knowledge graph.
                    let reflect_enabled = pool
                        .config
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|c| c.memory.cognitive_reflection)
                        .unwrap_or(true);
                    if reflect_enabled {
                        crate::agent::agent_pool::reflection::reflect_push(
                            &folder,
                            "Assistant",
                            &data.content,
                        );
                    }
                }),
            );
        }

        // ---- message:text:chunk (live reply streaming) ----
        //
        // Forwards each visible delta to the UI as `agent:delta`. The Web and
        // desktop clients accumulate it into a streaming bubble; voice chat
        // additionally carves it into sentences and starts TTS on the first one,
        // so the assistant begins talking while the model is still writing.
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let jid_arg = jid.clone();
            self.core_api.on_text_chunk(
                &jid_arg,
                Box::new(move |data: crate::zen_core::TextChunkData| {
                    if data.delta.is_empty() {
                        return;
                    }
                    if let Some(sink) = pool.agent_event_sink.lock().unwrap().as_ref() {
                        sink.notify_agent_delta(&jid, &data.delta);
                    }
                }),
            );
        }

        // ---- state:update ----
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let last_reply = Arc::clone(&last_reply);
            let jid_arg = jid.clone();
            self.core_api.on_state_update(
                &jid_arg,
                Box::new(move |data: StateUpdateData| {
                    let dispatch_task = pool
                        .state
                        .lock()
                        .unwrap()
                        .dispatch_task_map
                        .get(&jid)
                        .cloned()
                        .unwrap_or_else(|| "-".into());
                    tracing::info!(
                        "[AgentPool] state_update jid={jid} dispatch_task={dispatch_task} state={}",
                        data.state
                    );
                    if let Some(sink) = pool.agent_event_sink.lock().unwrap().as_ref() {
                        sink.notify_agent_state(&jid, &data.state);
                    }
                    if data.state == "idle" {
                        // Dispatch completion is handled in `process_and_wait_inner` after PAW
                        // observes `ProcessEvent::Idle`, so LLM/session errors do not race ahead
                        // and mark dispatch tasks done with an empty reply.
                        if pool.state.lock().unwrap().dispatch_executing.contains(&jid) {
                            *last_reply.lock().unwrap() = String::new();
                        }
                        // Forward to active process_and_wait event loop.
                        if let Some(tx) = pool
                            .state
                            .lock()
                            .unwrap()
                            .process_event_txs
                            .get(&jid)
                            .cloned()
                        {
                            let _ = tx.send(ProcessEvent::Idle);
                        }
                    } else if data.state == "paused" {
                        // Paused: don't send Reset (suspends inactivity timer in PAW).
                    } else {
                        // Processing / other active states — restart PAW inactivity timer.
                        if let Some(tx) = pool
                            .state
                            .lock()
                            .unwrap()
                            .process_event_txs
                            .get(&jid)
                            .cloned()
                        {
                            let _ = tx.send(ProcessEvent::Reset);
                        }
                    }
                }),
            );
        }

        // ---- todos:update ----
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let name = _name.clone();
            let jid_arg = jid.clone();
            self.core_api.on_todos_update(
                &jid_arg,
                Box::new(move |data: Vec<TodosUpdateItem>| {
                    tracing::info!(
                        "[AgentPool] todos_update handler jid={jid} items={}",
                        data.len()
                    );
                    let todos: Vec<TodoSnapshot> = data
                        .into_iter()
                        .map(|item| TodoSnapshot {
                            content: item.content,
                            status: item.status,
                            active_form: item.active_form,
                        })
                        .collect();
                    // Diff against the previous snapshot to find todos that
                    // just transitioned. Done BEFORE we replace cached_todos.
                    let prev_snapshot: Vec<TodoSnapshot> = {
                        let s = pool.state.lock().unwrap();
                        s.cached_todos
                            .get(&jid)
                            .map(|c| c.todos.clone())
                            .unwrap_or_default()
                    };
                    let _ = diff_todo_transitions(&prev_snapshot, &todos);
                    {
                        let mut s = pool.state.lock().unwrap();
                        s.cached_todos.insert(
                            jid.clone(),
                            CachedTodos {
                                agent_name: name.clone(),
                                todos: todos.clone(),
                            },
                        );
                    }
                    if let Some(sink) = pool.agent_event_sink.lock().unwrap().as_ref() {
                        sink.notify_agent_todos(&jid, &name, &todos);
                    } else {
                        tracing::warn!(
                            "[AgentPool] todos_update handler for {jid}: NO agent_event_sink set"
                        );
                    }
                }),
            );
        }

        // ---- compact:start ----
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let jid_arg = jid.clone();
            self.core_api.on_compact_start(
                &jid_arg,
                Box::new(move |_data: CompactStartData| {
                    if let Some(sink) = pool.agent_event_sink.lock().unwrap().as_ref() {
                        sink.notify_agent_compacting(&jid, true);
                    }
                }),
            );
        }

        // ---- compact:exec ----
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let folder = _folder.clone();
            let jid_arg = jid.clone();
            self.core_api.on_compact_exec(
                &jid_arg,
                Box::new(move |data: CompactExecData| {
                    if let Some(sink) = pool.agent_event_sink.lock().unwrap().as_ref() {
                        sink.notify_agent_compacting(&jid, false);
                    }

                    // ---- curated-memory consolidation (`memoryRecall`) ----
                    // The history that compaction just replaced lives on only
                    // in `data.summary`. When the toggle is on, distill it
                    // into curated memory files (LLM if configured, verbatim
                    // fallback) so it stays recallable. Fire-and-forget:
                    // failures log and never affect the turn.
                    if let Some(summary) = data.summary.clone() {
                        let cfg = pool.config.lock().unwrap().as_ref().cloned();
                        if let Some(cfg) = cfg {
                            let enabled = crate::gateway::group_manager::get_memory_recall_enabled(
                                &cfg.paths.global_config_path,
                            );
                            if enabled {
                                let folder = folder.clone();
                                let jid_for_origin = jid.clone();
                                let base = cfg.paths.agents_dir.join(&folder);
                                tokio::spawn(async move {
                                    let llm =
                                        crate::memory::cognitive::llm_openai::create_cognitive_llm(
                                            &cfg,
                                        );
                                    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
                                    match crate::memory::consolidate::consolidate_summary(
                                        &base, &jid_for_origin, &summary, llm, &date,
                                    )
                                    .await
                                    {
                                        Ok(n) if n > 0 => {
                                            // New files → let the watcher pick
                                            // them up on the next search.
                                            crate::memory::manager::get_instance()
                                                .mark_dirty(&folder, None);
                                        }
                                        Ok(_) => {}
                                        Err(e) => tracing::warn!(
                                            "[AgentPool] history consolidation failed: {e}"
                                        ),
                                    }
                                });
                            }
                        }
                    }

                    let today: String = chrono::Utc::now().format("%Y-%m-%d").to_string();
                    let changed_file = dirs::home_dir()
                        .map(|h| {
                            h.join("senclaw")
                                .join("agents")
                                .join(&folder)
                                .join("memory")
                                .join(format!("{today}.md"))
                        })
                        .unwrap_or_else(|| {
                            std::path::PathBuf::from("senclaw")
                                .join("agents")
                                .join(&folder)
                                .join("memory")
                                .join(format!("{today}.md"))
                        });
                    let mgr = crate::memory::manager::get_instance();
                    let changed_str = changed_file.to_string_lossy().to_string();
                    mgr.mark_dirty(&folder, Some(&changed_str));
                }),
            );
        }

        // ---- session:error ----
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let jid_arg = jid.clone();
            self.core_api.on_session_error(
                &jid_arg,
                Box::new(move |data: SessionErrorData| {
                    tracing::error!(
                        "[AgentPool] Session error for {jid}: [{code}] {msg}",
                        jid = jid,
                        code = data.code,
                        msg = data.message
                    );
                    // Forward to active process_and_wait event loop.
                    if let Some(tx) = pool
                        .state
                        .lock()
                        .unwrap()
                        .process_event_txs
                        .get(&jid)
                        .cloned()
                    {
                        let _ = tx.send(ProcessEvent::Error(data));
                    }
                }),
            );
        }

        // ---- tool:permission:request ----
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let chat_jid = _jid.clone();
            let bot_token = _bot_token.clone();
            let jid_arg = jid.clone();
            self.core_api.on_tool_permission_request(
                &jid_arg,
                Box::new(move |data: ToolPermissionRequestData| {
                    if let Some(pb) = pool.permission_bridge.lock().unwrap().as_ref() {
                        pb.handle_permission_request_gated(
                            &data.tool_name,
                            &data.permission_key,
                            &data.title,
                            &data.content,
                            &data.options,
                            &jid,
                            &chat_jid,
                            bot_token.as_deref(),
                        );
                    }
                }),
            );
        }

        // ---- ask:question:request ----
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let chat_jid = _jid.clone();
            let bot_token = _bot_token.clone();
            let jid_arg = jid.clone();
            self.core_api.on_ask_question_request(
                &jid_arg,
                Box::new(move |data: AskQuestionRequestData| {
                    if let Some(pb) = pool.permission_bridge.lock().unwrap().as_ref() {
                        pb.handle_ask_question_request(
                            &data.agent_id,
                            data.questions.clone(),
                            &jid,
                            &chat_jid,
                            bot_token.as_deref(),
                        );
                    }
                }),
            );
        }

        // ---- form:request ----
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let chat_jid = _jid.clone();
            let bot_token = _bot_token.clone();
            let jid_arg = jid.clone();
            self.core_api.on_form_request(
                &jid_arg,
                Box::new(move |data: crate::zen_core::FormRequestData| {
                    if let Some(pb) = pool.permission_bridge.lock().unwrap().as_ref() {
                        pb.handle_form_request(&data, &jid, &chat_jid, bot_token.as_deref());
                    }
                }),
            );
        }

        // ---- conversation:usage ----
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let jid_arg = jid.clone();
            self.core_api.on_conversation_usage(
                &jid_arg,
                Box::new(move |usage: crate::zen_core::ConversationUsageData| {
                    if let Some(sink) = pool.agent_event_sink.lock().unwrap().as_ref() {
                        sink.notify_agent_usage(&jid, usage);
                    }
                }),
            );
        }

        // ---- cleanup ----
        {
            let pool = Arc::clone(self);
            let jid = _jid.clone();
            let mut s = self.state.lock().unwrap();
            s.event_cleanups.insert(
                jid.clone(),
                Box::new(move || {
                    pool.core_api.off_all(&jid);
                }),
            );
        }
    }

    // ===== run_isolated (Phase 4) =====

    /// Run a scheduled task in an isolated core instance.
    /// Mirrors TS `runIsolated` (AgentPool.ts:839–929).
    ///
    /// Creates a fresh session, processes the prompt, and waits for idle or
    /// timeout. The real sema-core wiring (MCP servers, skills dirs) lands when
    /// the sema-core crate is available.
    pub async fn run_isolated(
        self: &Arc<Self>,
        task_id: &str,
        task_prompt: &str,
        group: &GroupBinding,
        prompt: Option<&str>,
    ) -> Result<()> {
        let effective_prompt = prompt.unwrap_or(task_prompt).to_string();
        let instance_id = format!("isolated-{task_id}");
        let _ = group;

        // Channel for the idle/timeout result.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Result<()>>();

        // Spawn a timeout + wait task.
        let task_id_owned = task_id.to_string();
        tokio::spawn(async move {
            let result = tokio::time::timeout(Duration::from_millis(AGENT_TIMEOUT_MS), async {
                // Poll until idle (placeholder — real sema-core emits state:update:idle).
                // For now just complete immediately with ok.
                Ok(())
            })
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "[AgentPool] Isolated task {task_id_owned} timed out"
                ))
            });
            let _ = done_tx.send(result);
        });

        let _ = self
            .core_api
            .process_user_input(&instance_id, &effective_prompt);

        done_rx
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Isolated task {task_id} aborted")))
    }

    // ===== workspace watcher + skills reload (Phase 4) =====

    /// Start watching the workspace state file for `jid`.
    /// Mirrors TS `setupWorkspaceWatcher` (AgentPool.ts:1273–1297).
    pub fn setup_workspace_watcher(self: &Arc<Self>, jid: &str, folder: &str) {
        let state_file = self.workspace_state_file(folder);
        let jid_owned = jid.to_string();
        let folder_owned = folder.to_string();
        let pool_weak = Arc::downgrade(self);
        let aborted = Arc::new(Mutex::new(false));

        // Store unwatch callback.
        {
            let mut s = self.state.lock().unwrap();
            let aborted_inner = Arc::clone(&aborted);
            s.workspace_watchers.insert(
                jid_owned.clone(),
                Box::new(move || {
                    *aborted_inner.lock().unwrap() = true;
                }),
            );
        }

        tokio::spawn(async move {
            let mut last_dir: Option<String> = None;
            loop {
                if *aborted.lock().unwrap() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                let raw = match std::fs::read_to_string(&state_file) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let state: Option<WorkspaceStateFile> = serde_json::from_str(&raw).ok();
                if let Some(state) = state {
                    let new_dir = state.current_dir;
                    if !new_dir.is_empty() && Some(&new_dir) != last_dir.as_ref() {
                        last_dir = Some(new_dir.clone());
                        if let Some(pool) = pool_weak.upgrade() {
                            pool.core_api.set_working_dir(&jid_owned, &new_dir);
                            {
                                let mut s = pool.state.lock().unwrap();
                                s.runtime_work_dirs
                                    .insert(jid_owned.clone(), new_dir.clone());
                            }
                            tracing::info!(
                                "[AgentPool] Workspace switched for {folder_owned}: {new_dir}"
                            );
                        }
                    }
                }
            }
        });
    }

    /// Watch skills for hot-reload. Two sources, one poller (mirrors TS
    /// `watchSkillsReloadSignal` + `watchManagedSkillsDir`,
    /// AgentPool.ts:214–260):
    ///
    /// 1. The clawhub reload-signal file
    ///    (`<managed_skills_dir>/.clawhub/reload-signal`) — written by
    ///    `senclaw clawhub install/uninstall/refresh` and UI enable/disable.
    ///    (The previous watcher looked for `<config dir>/.skills-reload`,
    ///    a path nothing writes — and was never spawned.)
    /// 2. The managed skills directory itself — a user manually `cp`ing or
    ///    deleting a skill folder writes no signal, so the dir's direct
    ///    children are fingerprinted; a change that stays stable for two
    ///    consecutive ticks (debounce for multi-file copies) triggers a
    ///    reload. `.clawhub` bookkeeping entries are ignored, and a
    ///    signal-triggered reload re-baselines the fingerprint so a clawhub
    ///    install doesn't reload twice.
    pub fn watch_skills_reload(self: &Arc<Self>, managed_skills_dir: std::path::PathBuf) {
        let signal_path = managed_skills_dir.join(".clawhub").join("reload-signal");
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            let mut first = true;
            let mut last_signal_mtime: Option<std::time::SystemTime> = None;
            let mut last_fp: u64 = 0;
            let mut pending_fp: Option<u64> = None;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let mut reloaded = false;

                // 1. Authoritative reload signal.
                match std::fs::metadata(&signal_path) {
                    Ok(meta) => {
                        let mtime = meta.modified().ok();
                        if !first && mtime != last_signal_mtime {
                            pool.reload_all_skills();
                            reloaded = true;
                        }
                        last_signal_mtime = mtime;
                    }
                    Err(_) => last_signal_mtime = None,
                }

                // 2. Manual add/remove of skill folders (no signal written).
                let fp = fingerprint_skills_dir(&managed_skills_dir);
                if first || reloaded {
                    last_fp = fp;
                    pending_fp = None;
                } else if fp != last_fp {
                    if pending_fp == Some(fp) {
                        last_fp = fp;
                        pending_fp = None;
                        tracing::info!(
                            "[AgentPool] managed skills dir changed on disk; reloading skills"
                        );
                        pool.reload_all_skills();
                    } else {
                        pending_fp = Some(fp); // debounce until stable
                    }
                } else {
                    pending_fp = None;
                }
                first = false;
            }
        });
    }

    /// Queue a user input into the engine's mid-turn pending queue if the
    /// group's agent is currently processing. Returns `false` when idle —
    /// callers then dispatch via the normal GroupQueue full-turn path (which
    /// runs memory pre-retrieval etc.). Injected inputs ride the in-flight
    /// turn, so skipping those per-turn pre-stages is correct for them.
    pub fn queue_input_if_processing(&self, jid: &str, prompt: &str) -> bool {
        self.core_api.queue_input_if_processing(jid, prompt)
    }

    /// Reload skills across all active cores. Mirrors TS `reloadAllSkills`
    /// (AgentPool.ts:190–222).
    pub fn reload_all_skills(&self) {
        let jids: Vec<String> = {
            let s = self.state.lock().unwrap();
            s.cores.iter().cloned().collect()
        };
        let count = jids.len();
        if count == 0 {
            tracing::info!("[AgentPool] skills reload signal received (no active agents)");
            return;
        }
        tracing::info!("[AgentPool] Reloading skills for {count} active agent(s)");
        let disabled = crate::skills::disabled::read_disabled_skills();
        for _jid in &jids {
            self.core_api.reload_skills(&disabled);
        }
    }

    /// Re-read hook config for every live agent.
    ///
    /// Called after anything changes what is on disk: installing or removing a
    /// kit, and saving the Hooks page. Without it the change reaches only
    /// conversations started afterwards.
    pub fn reload_all_hooks(&self) {
        let count = self.state.lock().unwrap().cores.len();
        if count == 0 {
            tracing::info!("[AgentPool] hooks reload signal received (no active agents)");
            return;
        }
        tracing::info!("[AgentPool] Reloading hooks for {count} active agent(s)");
        self.core_api.reload_hooks();
    }

    // ===== Feishu credentials (Phase 4) =====

    /// Resolve Feishu app credentials for a given bot token.
    /// Mirrors TS `resolveFeishuCredentials` (AgentPool.ts:1232–1251).
    ///
    /// If `bot_token` is provided, looks up the matching app in the global
    /// config's `feishu_apps` map. Falls back to env-var credentials.
    pub fn resolve_feishu_credentials(
        &self,
        config_path: &std::path::Path,
        feishu_config: &crate::config::FeishuConfig,
        bot_token: Option<&str>,
    ) -> Option<super::workspace::FeishuCredentials> {
        if let Some(token) = bot_token {
            let apps = crate::gateway::group_manager::get_feishu_apps(config_path);
            if let Some(app) = apps.get(token) {
                return Some(super::workspace::FeishuCredentials {
                    app_id: token.to_string(),
                    app_secret: app.app_secret.clone(),
                    domain: app.domain.clone(),
                });
            }
        }
        if !feishu_config.app_id.is_empty() && !feishu_config.app_secret.is_empty() {
            return Some(super::workspace::FeishuCredentials {
                app_id: feishu_config.app_id.clone(),
                app_secret: feishu_config.app_secret.clone(),
                domain: Some(feishu_config.domain.clone()),
            });
        }
        tracing::warn!("[AgentPool] Cannot resolve Feishu credentials for botToken={bot_token:?}");
        None
    }
}

/// One todo whose `status` changed between two snapshots — used to drive
/// progress messages into the cowork chat panel.
#[derive(Debug, Clone)]
#[allow(dead_code)] // consumed by tests only until the cowork progress feed reads it
pub(crate) struct TodoTransition {
    pub content: String,
    pub active_form: Option<String>,
    pub from: Option<String>,
    pub to: String,
}

/// Compare two ordered lists of todos and return entries whose status
/// changed. Matching is by `content` because there's no stable ID. New
/// items (no previous match) are treated as transitions from `None`.
pub(crate) fn diff_todo_transitions(
    prev: &[TodoSnapshot],
    next: &[TodoSnapshot],
) -> Vec<TodoTransition> {
    let mut out = Vec::new();
    for n in next {
        let prev_status = prev
            .iter()
            .find(|p| p.content == n.content)
            .map(|p| p.status.clone());
        if prev_status.as_deref() != Some(n.status.as_str()) {
            out.push(TodoTransition {
                content: n.content.clone(),
                active_form: n.active_form.clone(),
                from: prev_status,
                to: n.status.clone(),
            });
        }
    }
    out
}

/// Cognitive-memory pre-retrieval. Spreading-activation recall scoped to the
/// caller's group folder, with Hebbian write-back happening as a side effect.
/// Returns an empty string when:
///   * the cognitive system was never booted (no embedding provider, etc.)
///   * no hits cross the relevance floor
///   * any error occurs — pre-retrieval is **never** allowed to fail the
///     surrounding agent turn, so failures log + drop quietly.
async fn cognitive_pre_retrieval(prompt: &str, group_folder: &str, max_results: usize) -> String {
    let Some(sys) = crate::memory::cognitive::try_get_instance() else {
        return String::new();
    };
    let mut q =
        crate::memory::cognitive::SearchQuery::spreading(prompt.to_string(), max_results, 2);
    q.node_sets = vec![crate::memory::cognitive::NodeSet::group(
        group_folder,
        "default_memory",
    )];
    match sys.search(&q).await {
        Ok(hits) => {
            // Drop very-low-confidence hits — at <0.1 score the LLM context
            // budget is better spent on the user prompt itself.
            let filtered: Vec<_> = hits.into_iter().filter(|h| h.score >= 0.1).collect();
            crate::memory::cognitive::format_hits_for_prompt(&filtered, 200)
        }
        Err(e) => {
            tracing::warn!("[AgentPool] Cognitive pre-retrieval failed: {e}");
            String::new()
        }
    }
}

/// Curated-memory pre-retrieval (the `memoryRecall` toggle). Hybrid FTS5 +
/// vector search over the group's curated `memory/*.md` files via the
/// MemoryManager singleton, deduped per file and presented per-memory
/// (`name (type) — hook` + matched snippet), ending with a hint that the
/// agent can call the memory tools for deeper retrieval.
///
/// Session scoping: memories whose `originSessionId` is another chat JID are
/// skipped (prevents bleed between chats that share an agent folder). Same-JID
/// hits are preferred over agent-wide / legacy folder-tagged ones.
///
/// Mirrors `cognitive_pre_retrieval`'s failure contract: any error logs and
/// returns an empty string — pre-retrieval never fails the agent turn.
async fn curated_pre_retrieval(
    query: &str,
    group_folder: &str,
    chat_jid: &str,
    max_results: usize,
) -> String {
    let mgr = crate::memory::manager::get_instance();
    let opts = crate::memory::fts_search::SearchOptions {
        max_results: (max_results + 5) * 2,
        min_score: 0.25,
        source: Some("memory".to_owned()),
    };
    let results = match mgr.search(group_folder, query, Some(opts)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[AgentPool] Curated pre-retrieval failed: {e}");
            return String::new();
        }
    };

    // Curated files only: skip daily logs (YYYY-MM-DD.md) and the index.
    let today_file = format!("{}.md", chrono::Utc::now().format("%Y-%m-%d"));
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ranked: Vec<(i32, String, String)> = Vec::new();
    for r in results {
        let file_name = std::path::Path::new(&r.path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        if file_name == today_file
            || file_name == "MEMORY.md"
            || is_daily_log_name(&file_name)
            || !seen.insert(r.path.clone())
        {
            continue;
        }
        let meta = crate::memory::curated::read_meta(std::path::Path::new(&r.path));
        let rank = match &meta {
            Some(m) => crate::memory::curated::memory_session_rank(
                &m.origin_session_id,
                chat_jid,
                group_folder,
            ),
            None => 1, // no frontmatter → treat as shared
        };
        if rank < 0 {
            continue; // other chat's session-scoped memory
        }
        let header = match meta {
            Some(m) => format!(
                "{} ({}) — {}",
                m.name,
                if m.mem_type.is_empty() {
                    "memory"
                } else {
                    &m.mem_type
                },
                m.description
            ),
            None => file_name,
        };
        let snippet: String = r.text.chars().take(300).collect();
        ranked.push((rank, header, snippet));
    }

    // Same-session first, then shared; preserve search relevance within a tier.
    ranked.sort_by(|a, b| b.0.cmp(&a.0));

    let mut out = String::new();
    let mut n = 0usize;
    for (_rank, header, snippet) in ranked.into_iter().take(max_results) {
        n += 1;
        out.push_str(&format!("[{n}] {header}\n{snippet}\n\n"));
    }

    if n == 0 {
        return String::new();
    }
    format!(
        "Saved memories relevant to this request (point-in-time notes — verify before asserting):\n\n{}\n\nIf you need more detail, call the memory_recall tool (search) or memory_get (read a full memory file).",
        out.trim()
    )
}

/// `YYYY-MM-DD.md` daily-log filename check (any date, not just today).
fn is_daily_log_name(file_name: &str) -> bool {
    let Some(stem) = file_name.strip_suffix(".md") else {
        return false;
    };
    let b = stem.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && stem
            .chars()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7) || c.is_ascii_digit())
}

/// Filter for the auto-reflection path (P14).
///
/// We're choosing what to feed the LLM-driven cognify pipeline on every
/// user turn. Three failure modes to guard against:
///
///   * **Noise** — one-word acknowledgements ("ok", "yes", "thanks") run
///     up LLM cost for zero useful triplets.
///   * **Questions** — questions are queries, not facts. Cognifying
///     "do you know where my keys are?" would persist a bogus
///     (you, know_where, my-keys) triplet.
///   * **Paste bombs** — a user pasting a 50 KB log shouldn't trigger
///     cognify; the prompt blowup would tie up the local model for minutes
///     extracting nonsense. The agent can still call CogAdd explicitly.
///
/// `min_chars`/`max_chars` come from `CognitiveConfig` so users can tune
/// per environment. Heuristic, not exhaustive — false positives are cheap
/// because cognify dedupes by content hash.
pub(crate) fn should_reflect(text: &str, min_chars: usize, max_chars: usize) -> bool {
    let t = text.trim();
    let n = t.chars().count();
    if n < min_chars || n > max_chars {
        return false;
    }
    // Pure-question heuristic: ends with question mark AND there's only one
    // sentence. "Where is X? It's on the table." → still reflect (statement
    // present). "Where is X?" alone → skip.
    let only_question =
        t.ends_with('?') && !t.contains(". ") && !t.contains("。") && !t.contains('.');
    !only_question
}

// NOTE: the per-message `cognitive_reflect` was replaced by the session
// window in `agent_pool::reflection` (P14 v2) — turns are buffered per
// group and cognified as one call on size/idle flush. `should_reflect`
// above is still the window-level gate.

#[cfg(test)]
mod merge_reasoning_tests {
    use super::merge_assistant_reasoning_for_web_ui;

    /// Gemma-4: parser extracted `<|channel>thought\n…<channel|>` content into
    /// reasoning; visible content has no `<think>`. The merge must wrap the
    /// reasoning so the web UI renders a collapsible thinking block.
    // Convention for the rest of this module: `_final` = end-of-conversation
    // turn (no tool_calls); `_intermediate` = thinking-then-tool turn.

    #[test]
    fn wraps_gemma4_reasoning_into_think_block() {
        let reasoning = "The user wants gold price. I will search.";
        let content = "Here is the answer.";
        let out = merge_assistant_reasoning_for_web_ui(reasoning, content, false);
        assert!(
            out.starts_with("<think>\n"),
            "expected leading <think>, got: {out:?}"
        );
        assert!(
            out.contains("</think>"),
            "expected closing </think>: {out:?}"
        );
        assert!(out.contains(reasoning), "reasoning lost: {out:?}");
        assert!(out.contains(content), "content lost: {out:?}");
    }

    /// When the model itself already wrote a leading `<think>…</think>` block
    /// (Qwen with raw text streaming), don't double-wrap.
    #[test]
    fn does_not_double_wrap_when_content_already_has_leading_think() {
        let out = merge_assistant_reasoning_for_web_ui(
            "outer reasoning",
            "<think>inner</think>\n\nThe answer.",
            false,
        );
        assert_eq!(out, "<think>inner</think>\n\nThe answer.");
    }

    /// Regression: previously, ANY occurrence of `<think` in the first 4 KB
    /// suppressed wrapping — including code examples in the visible answer.
    /// That left Gemma-4 thinking invisible. The new guard only skips on a
    /// LEADING block, so reasoning still wraps when `<think>` appears mid-body.
    #[test]
    fn wraps_when_think_appears_only_mid_content() {
        let content = "To enable thinking, use the <think> tag in your prompt.";
        let out = merge_assistant_reasoning_for_web_ui("model reasoning", content, false);
        assert!(
            out.starts_with("<think>\nmodel reasoning\n</think>"),
            "incidental <think in body must not block wrapping: {out:?}"
        );
        assert!(out.ends_with(content));
    }

    #[test]
    fn empty_reasoning_passes_content_through() {
        let out = merge_assistant_reasoning_for_web_ui("", "Just an answer.", false);
        assert_eq!(out, "Just an answer.");
    }

    /// Regression for daemon panic at `pool.rs:62` — `byte index 64 is not a
    /// char boundary; it is inside 'đ'`. Vietnamese (and any multi-byte UTF-8)
    /// content with `đ`/`ă`/`ơ` straddling the prefix-check boundary used to
    /// panic the entire merge function → assistant message never broadcast →
    /// UI froze with raw harmony markers from the previous turn.
    #[test]
    fn does_not_panic_on_multibyte_utf8_at_prefix_boundary() {
        // Vietnamese content where `đ` (2 bytes) straddles byte 64 — same shape
        // as the real-session crash payload `**Giá vàng hôm nay:**\n\n...`.
        let content = "**Giá vàng hôm nay:**\n\nDưới đây là một số mức giá tham khảo: \
                       Vàng SJC giao dịch khoảng 75 triệu đồng mỗi lượng.";
        // Sanity: trigger the prefix-check path (long enough that the bug fires).
        assert!(content.len() > 64);
        // Must NOT panic; reasoning gets wrapped, content preserved.
        let out = merge_assistant_reasoning_for_web_ui("thinking about prices", content, false);
        assert!(out.starts_with("<think>\n"), "expected wrap, got: {out:?}");
        assert!(out.contains("Giá vàng hôm nay"), "content lost: {out:?}");
    }

    /// FINAL turn (no tool calls) but body still empty — model's answer is
    /// presumably trapped inside an unclosed channel. Show the reasoning as
    /// the body so the user sees *something*.
    #[test]
    fn empty_content_no_tool_calls_surfaces_reasoning_as_body() {
        let out = merge_assistant_reasoning_for_web_ui(
            "All the thinking and the answer got stuck in one block.",
            "",
            false, // ← FINAL turn
        );
        // No <think> wrap — direct content so the user sees it.
        assert!(
            !out.starts_with("<think>"),
            "expected raw body, got: {out:?}"
        );
        assert!(out.contains("All the thinking"));
    }

    /// INTERMEDIATE turn (model emits thinking + tool_call, no user-facing
    /// answer yet). The empty body is EXPECTED — the next turn will produce
    /// the answer after the tool runs. Wrap reasoning into `<think>` so the
    /// UI shows ONLY the collapsible widget (body stays empty). Without this
    /// fix, the Layer 2 fallback would dump ~1000 chars of "I will invoke X"
    /// thinking on the user between every tool turn.
    #[test]
    fn empty_content_with_tool_calls_keeps_reasoning_collapsed() {
        let reasoning = "The user wants today's gold price. I should invoke the \
                         agent-browser skill to search the web.";
        let out = merge_assistant_reasoning_for_web_ui(reasoning, "", true);
        assert!(
            out.starts_with("<think>\n"),
            "expected <think> wrap: {out:?}"
        );
        assert!(out.contains("</think>"), "expected closing </think>");
        assert!(out.contains(reasoning), "reasoning lost: {out:?}");
        // No body after </think> — the tool-execution chip rendered next in the
        // UI tells the user what's happening. Don't dump the raw reasoning.
        let after_close = out.split("</think>").nth(1).unwrap_or("");
        assert!(
            after_close.trim().is_empty(),
            "intermediate turn must not have body after </think>, got: {after_close:?}"
        );
    }
}

/// Order-independent fingerprint of a skills directory's direct children
/// (name + mtime), ignoring `.clawhub` bookkeeping. Used by
/// [`AgentPool::watch_skills_reload`] to detect manual skill add/remove.
fn fingerprint_skills_dir(dir: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut entries: Vec<(String, Option<std::time::SystemTime>)> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with(".clawhub") {
                    return None;
                }
                let mtime = e.metadata().ok().and_then(|m| m.modified().ok());
                Some((name, mtime))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    entries.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    entries.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod fingerprint_tests {
    use super::fingerprint_skills_dir;

    #[test]
    fn detects_added_and_removed_entries() {
        let dir = tempfile::tempdir().unwrap();
        let base = fingerprint_skills_dir(dir.path());
        std::fs::create_dir(dir.path().join("my-skill")).unwrap();
        let with_skill = fingerprint_skills_dir(dir.path());
        assert_ne!(base, with_skill);
        std::fs::remove_dir(dir.path().join("my-skill")).unwrap();
        assert_eq!(fingerprint_skills_dir(dir.path()), base);
    }

    #[test]
    fn ignores_clawhub_bookkeeping() {
        let dir = tempfile::tempdir().unwrap();
        let base = fingerprint_skills_dir(dir.path());
        std::fs::create_dir(dir.path().join(".clawhub")).unwrap();
        std::fs::write(dir.path().join(".clawhub").join("reload-signal"), "x").unwrap();
        assert_eq!(fingerprint_skills_dir(dir.path()), base);
    }

    #[test]
    fn missing_dir_is_stable() {
        let ghost = std::path::Path::new("/nonexistent/senclaw-skills-test");
        assert_eq!(fingerprint_skills_dir(ghost), fingerprint_skills_dir(ghost));
    }
}

#[cfg(test)]
mod reflection_tests {
    use super::should_reflect;
    const MIN: usize = 20;
    const MAX: usize = 2000;

    #[test]
    fn skip_short_messages() {
        assert!(!should_reflect("ok", MIN, MAX));
        assert!(!should_reflect("yes", MIN, MAX));
        assert!(!should_reflect("thanks!", MIN, MAX));
    }

    #[test]
    fn skip_pure_questions() {
        assert!(!should_reflect("where are my keys right now?", MIN, MAX));
        assert!(!should_reflect("bạn có biết tôi tên gì không?", MIN, MAX));
    }

    #[test]
    fn skip_paste_bombs() {
        // 3000-char paste — beyond MAX (2000). Cognify would chew through
        // it for nothing useful; user can CogAdd manually if needed.
        let long = "x".repeat(3000);
        assert!(!should_reflect(&long, MIN, MAX));
    }

    #[test]
    fn accept_factual_statements() {
        assert!(should_reflect(
            "tôi tên là Sen, sống ở Hà Nội và thích cà phê đen.",
            MIN,
            MAX,
        ));
        assert!(should_reflect(
            "Today I learned that Rust has a borrow checker.",
            MIN,
            MAX,
        ));
    }

    #[test]
    fn accept_mixed_statements_with_trailing_question() {
        // Mixed content — there's a fact AND a question. Reflect anyway
        // since the cognify pipeline will pull triplets only from the
        // statement part.
        assert!(should_reflect(
            "My phone is 0901234567. What's yours?",
            MIN,
            MAX
        ));
    }
}

#[cfg(test)]
mod todo_transition_tests {
    use super::{diff_todo_transitions, TodoSnapshot};

    fn t(content: &str, status: &str) -> TodoSnapshot {
        TodoSnapshot {
            content: content.into(),
            status: status.into(),
            active_form: None,
        }
    }

    #[test]
    fn detects_new_item_as_transition_from_none() {
        let prev: Vec<TodoSnapshot> = vec![];
        let next = vec![t("Frame scope", "pending")];
        let diffs = diff_todo_transitions(&prev, &next);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].from, None);
        assert_eq!(diffs[0].to, "pending");
    }

    #[test]
    fn detects_status_change() {
        let prev = vec![t("Search Shopee", "pending"), t("Synth", "pending")];
        let next = vec![t("Search Shopee", "in_progress"), t("Synth", "pending")];
        let diffs = diff_todo_transitions(&prev, &next);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].content, "Search Shopee");
        assert_eq!(diffs[0].from.as_deref(), Some("pending"));
        assert_eq!(diffs[0].to, "in_progress");
    }

    #[test]
    fn ignores_unchanged_items() {
        let prev = vec![t("A", "completed"), t("B", "in_progress")];
        let next = vec![t("A", "completed"), t("B", "in_progress")];
        assert!(diff_todo_transitions(&prev, &next).is_empty());
    }

    #[test]
    fn detects_completion() {
        let prev = vec![t("A", "in_progress")];
        let next = vec![t("A", "completed")];
        let diffs = diff_todo_transitions(&prev, &next);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].to, "completed");
    }
}

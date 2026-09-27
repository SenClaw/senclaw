//! Virtual worker pool — temporary virtual-agent instance manager.
//! Port target: src-old/agent/VirtualWorkerPool.ts
//!
//! Creates temporary sema-core instances per [`PersonaConfig`] and destroys them after
//! prompt execution. Uses per-persona concurrency limits and supports `cancel_all` /
//! `cancel_task` for force-stop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::agent::persona_registry::PersonaConfig;
use crate::mcp::manager::McpManager;
use crate::zen_core::McpServerConfig;

// ===== Types =====

/// Result of a virtual agent run.
#[derive(Debug, Clone)]
pub struct VirtualRunResult {
    pub result: String,
    pub duration_ms: u64,
    /// Total billed input/output tokens across every LLM call of the run
    /// (0 when the provider reported no usage, e.g. local models pre-wiring).
    /// The per-call rows are already in `llm_usage_log` via the engine bus —
    /// these totals exist so `agent.run` can hand the number straight back to
    /// the calling Space App without re-querying.
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// Shared token accumulator threaded into `execute_virtual_prompt` so the
/// event loop can total up `EngineEvent::LlmUsage` for the run result.
#[derive(Default)]
pub struct UsageAccum {
    pub tokens_in: std::sync::atomic::AtomicU64,
    pub tokens_out: std::sync::atomic::AtomicU64,
}

/// A todo item pushed from the virtual agent.
#[derive(Debug, Clone, Serialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
    #[serde(
        default,
        rename = "activeForm",
        skip_serializing_if = "Option::is_none"
    )]
    pub active_form: Option<String>,
}

/// Callback for forwarding virtual-agent todos to WsGateway.
pub type TodosNotifyFn = Arc<dyn Fn(&str, &str, &[TodoItem]) + Send + Sync>;

/// A single activity entry emitted by a virtual sub-agent (tool call, thinking, message).
/// Forwarded to admin WS clients as `dispatch:activity`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubAgentActivityEntry {
    pub entry_type: String, // "tool" | "think" | "message"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>, // for think/message entries
    pub ts: String,
}

/// Callback for forwarding virtual-agent activity (tool calls, thinking)
/// to the admin UI. Arguments: (task_id, entry)
pub type ActivityNotifyFn = Arc<dyn Fn(&str, SubAgentActivityEntry) + Send + Sync>;

/// Callback wired from lib.rs so virtual agents can surface permission requests to admin UI.
///
/// Arguments: (virtual_jid, tool_name, title, content, options, response_sender)
/// Caller stores the sender, notifies UI via PermissionBridge, and delivers the
/// selected option string back through the sender when the user responds.
pub type VirtualPermissionFn = Arc<
    dyn Fn(
            String,                              // virtual_jid
            String,                              // tool_name
            String,                              // permission_key
            String,                              // title
            serde_json::Value,                   // content
            HashMap<String, String>,             // options
            std::sync::mpsc::SyncSender<String>, // one-shot response channel
        ) + Send
        + Sync,
>;

// ===== Trait — abstracts sema-core lifecycle for virtual agents =====

/// Abstracts the full sema-core lifecycle for a virtual agent run:
/// create → createSession → processUserInput → wait for idle → dispose.
#[allow(unused_variables)]
pub trait VirtualCoreApi: Send + Sync {
    /// Execute a prompt in a new temporary sema-core instance.
    ///
    /// Implementations should:
    /// 1. Create SemaCore with the given config
    /// 2. Create a session
    /// 3. Bind permission events if `on_permission_bind` is set
    /// 4. Process user input
    /// 5. Wait for `state:update` (idle), `session:error`, or cancellation
    /// 6. Dispose the core
    /// 7. Return the final reply text from `message:complete` (agentId === 'main')
    fn execute_virtual_prompt(
        &self,
        instance_id: &str,
        agent_data_dir: &str,
        working_dir: &str,
        tools: &[String],
        system_prompt: Option<&str>,
        skills_extra_dirs: &[String],
        skip_perms: bool,
        prompt: &str,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
        todos_notify: Option<TodosNotifyFn>,
        extra_mcp_servers: &[McpServerConfig],
        virtual_jid: &str,
        permission_fn: Option<VirtualPermissionFn>,
        custom_memory_dir: Option<&str>,
        memory_folder_override: Option<&str>,
        activity_notify: Option<ActivityNotifyFn>,
        usage_accum: Option<Arc<UsageAccum>>,
    ) -> Result<String> {
        let _ = usage_accum;
        Err(anyhow::anyhow!("VirtualCoreApi not wired"))
    }
}

// ===== Default config =====

const DEFAULT_TIMEOUT_MS: u64 = 10 * 60 * 1000; // 10 minutes

/// Default agent-loop turn budget for virtual workers. Browser-driven research
/// burns ~2 turns per page (navigate + extract), so the global default of 30
/// kills legitimate multi-source scouting runs. Override with
/// `SENCLAW_VIRTUAL_MAX_TURNS`.
fn virtual_max_turns() -> usize {
    std::env::var("SENCLAW_VIRTUAL_MAX_TURNS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(60)
}

/// Tools excluded for virtual agents (Task, AskUserQuestion require main-agent context).
const VIRTUAL_EXCLUDED_TOOLS: &[&str] = &["Task", "AskUserQuestion"];

/// All non-admin pooled tools (used when persona doesn't specify a custom tool list).
const ALL_POOLED_TOOLS: &[&str] = &[
    "Bash",
    "Glob",
    "Grep",
    "Read",
    "Write",
    "Edit",
    "TodoWrite",
    "Skill",
    "NotebookEdit",
    "ExitPlanMode",
];

// ===== Internal: running instance tracking =====

struct RunningInstance {
    persona_name: String,
    cancel: Arc<AtomicBool>,
    /// cancel_task already decremented active_counts; skip duplicate decrement in finally.
    count_decremented_early: bool,
}

// ===== VirtualWorkerPool =====

pub struct VirtualWorkerPool {
    active_counts: Mutex<HashMap<String, u32>>,
    running_instances: Mutex<HashMap<String, RunningInstance>>,
    api: Arc<dyn VirtualCoreApi>,

    todos_notify: Mutex<Option<TodosNotifyFn>>,
    /// Extra MCP servers (e.g. browser-mcp) injected into every virtual engine.
    extra_mcp_servers: Mutex<Vec<McpServerConfig>>,
    /// Permission bridge callback: called to bind a virtual agent core for permission forwarding.
    /// Returns a cleanup function.
    on_bind_permission: Mutex<
        Option<Box<dyn Fn(&str, &str, bool) -> Option<Box<dyn FnOnce() + Send>> + Send + Sync>>,
    >,
    /// Returns the current skip-permission state (follows main-agent config).
    get_skip_perms: Mutex<Option<Arc<dyn Fn() -> bool + Send + Sync>>>,
    /// Forward permission requests from virtual agents to the admin UI.
    /// Set from lib.rs after PermissionBridge is available.
    permission_fn: Mutex<Option<VirtualPermissionFn>>,
    /// Forward virtual-agent tool/thinking activity to admin UI.
    activity_notify: Mutex<Option<ActivityNotifyFn>>,
}

impl VirtualWorkerPool {
    pub fn new(api: Arc<dyn VirtualCoreApi>) -> Self {
        Self {
            active_counts: Mutex::new(HashMap::new()),
            running_instances: Mutex::new(HashMap::new()),
            api,
            todos_notify: Mutex::new(None),
            extra_mcp_servers: Mutex::new(Vec::new()),
            on_bind_permission: Mutex::new(None),
            get_skip_perms: Mutex::new(None),
            permission_fn: Mutex::new(None),
            activity_notify: Mutex::new(None),
        }
    }

    pub fn set_activity_notify(&self, f: ActivityNotifyFn) {
        *self.activity_notify.lock().unwrap() = Some(f);
    }

    /// Set extra MCP servers to inject into every virtual engine (e.g. browser-mcp).
    pub fn set_extra_mcp_servers(&self, servers: Vec<McpServerConfig>) {
        *self.extra_mcp_servers.lock().unwrap() = servers;
    }

    // ===== Callback setters =====

    /// Inject todos-notify callback (forward virtual-agent todos to WsGateway).
    pub fn set_todos_notify(&self, f: TodosNotifyFn) {
        *self.todos_notify.lock().unwrap() = Some(f);
    }

    /// Inject the permission-forwarding callback so virtual agents can show
    /// approval prompts in the admin UI instead of silently auto-refusing.
    pub fn set_virtual_permission_fn(&self, f: VirtualPermissionFn) {
        *self.permission_fn.lock().unwrap() = Some(f);
    }

    /// Inject permission bridge binding.
    /// `on_bind`: receives (virtual_jid, persona_name, skip_perms), returns optional cleanup fn.
    pub fn set_permission_bind<
        F: Fn(&str, &str, bool) -> Option<Box<dyn FnOnce() + Send>> + Send + Sync + 'static,
    >(
        &self,
        on_bind: F,
        get_skip_perms: Arc<dyn Fn() -> bool + Send + Sync>,
    ) {
        *self.on_bind_permission.lock().unwrap() = Some(Box::new(on_bind));
        *self.get_skip_perms.lock().unwrap() = Some(get_skip_perms);
    }

    // ===== Public API =====

    /// Run a virtual agent with the given persona config and prompt.
    /// Returns the agent's final reply text and duration.
    pub async fn run(
        &self,
        persona: &PersonaConfig,
        prompt: &str,
        workspace_dir: &str,
        task_id: Option<&str>,
        timeout_override: Option<Duration>,
        custom_memory_dir: Option<&str>,
        memory_folder_override: Option<&str>,
    ) -> Result<VirtualRunResult> {
        // Concurrency gate
        let persona_name = &persona.name;
        {
            let mut counts = self.active_counts.lock().unwrap();
            let current = *counts.get(persona_name).unwrap_or(&0);
            if current >= persona.max_concurrent {
                bail!(
                    "Persona \"{persona_name}\" has reached max concurrency ({}). \
                     {current} instance(s) currently running.",
                    persona.max_concurrent
                );
            }
            counts.insert(persona_name.clone(), current + 1);
        }

        let start = std::time::Instant::now();
        let instance_id = generate_instance_id(persona_name);
        let temp_dir = std::env::temp_dir().join(format!("senclaw-virtual-{instance_id}"));

        let cancel = Arc::new(AtomicBool::new(false));
        let task_id = task_id.unwrap_or(&instance_id).to_string();

        // Register running instance for cancel tracking
        {
            let mut instances = self.running_instances.lock().unwrap();
            instances.insert(
                task_id.clone(),
                RunningInstance {
                    persona_name: persona_name.clone(),
                    cancel: cancel.clone(),
                    count_decremented_early: false,
                },
            );
        }

        // Prepare cleanup guard
        let cleanup_guard = CleanupGuard {
            pool: self,
            persona_name: persona_name.clone(),
            task_id: task_id.clone(),
            temp_dir: temp_dir.clone(),
            cleanup_permission: std::sync::Mutex::new(None::<Box<dyn FnOnce() + Send>>),
        };

        let usage_accum = Arc::new(UsageAccum::default());
        let result = self
            .execute_inner(
                persona,
                prompt,
                workspace_dir,
                &task_id,
                &instance_id,
                &temp_dir,
                cancel,
                timeout_override,
                &cleanup_guard,
                custom_memory_dir,
                memory_folder_override,
                Arc::clone(&usage_accum),
            )
            .await;

        // Cleanup runs via Drop, but we handle result explicitly
        let duration_ms = start.elapsed().as_millis() as u64;

        // Run cleanup
        cleanup_guard.run();

        match result {
            Ok(output) => Ok(VirtualRunResult {
                result: output,
                duration_ms,
                tokens_in: usage_accum
                    .tokens_in
                    .load(std::sync::atomic::Ordering::Relaxed),
                tokens_out: usage_accum
                    .tokens_out
                    .load(std::sync::atomic::Ordering::Relaxed),
            }),
            Err(e) => Err(e),
        }
    }

    /// Get the current active count for a persona.
    pub fn get_active_count(&self, persona_name: &str) -> u32 {
        *self
            .active_counts
            .lock()
            .unwrap()
            .get(persona_name)
            .unwrap_or(&0)
    }

    /// Force-stop a virtual task by task ID.
    pub fn cancel_task(&self, task_id: &str) {
        let (cancel, persona_name, needs_decrement) = {
            let mut instances = self.running_instances.lock().unwrap();
            let Some(instance) = instances.get_mut(task_id) else {
                return;
            };
            tracing::warn!(
                "[VirtualWorkerPool] Cancelling task {task_id} (persona: {})",
                instance.persona_name
            );
            let needs_decrement = !instance.count_decremented_early;
            if needs_decrement {
                instance.count_decremented_early = true;
            }
            (
                Arc::clone(&instance.cancel),
                instance.persona_name.clone(),
                needs_decrement,
            )
        };

        if needs_decrement {
            self.decrement_count(&persona_name);
        }

        cancel.store(true, Ordering::SeqCst);
    }

    /// Force-stop all running virtual tasks.
    pub fn cancel_all(&self) {
        let task_ids: Vec<String> = {
            let instances = self.running_instances.lock().unwrap();
            if instances.is_empty() {
                return;
            }
            tracing::warn!(
                "[VirtualWorkerPool] Cancelling all {} running instance(s)",
                instances.len()
            );
            instances.keys().cloned().collect()
        };
        for task_id in &task_ids {
            self.cancel_task(task_id);
        }
    }

    // ===== Internal =====

    async fn execute_inner(
        &self,
        persona: &PersonaConfig,
        prompt: &str,
        workspace_dir: &str,
        task_id: &str,
        instance_id: &str,
        temp_dir: &std::path::Path,
        cancel: Arc<AtomicBool>,
        timeout_override: Option<Duration>,
        cleanup_guard: &CleanupGuard<'_>,
        custom_memory_dir: Option<&str>,
        memory_folder_override: Option<&str>,
        usage_accum: Arc<UsageAccum>,
    ) -> Result<String> {
        // Prepare temp agent data dir
        tokio::fs::create_dir_all(temp_dir).await?;
        if !persona.system_prompt.is_empty() {
            tokio::fs::write(temp_dir.join("CLAUDE.md"), persona.system_prompt.as_bytes()).await?;
        }

        // Resolve tool set (exclude VIRTUAL_EXCLUDED_TOOLS)
        let tools: Vec<String> = if let Some(ref custom_tools) = persona.tools {
            custom_tools
                .iter()
                .filter(|t| !VIRTUAL_EXCLUDED_TOOLS.contains(&t.as_str()))
                .cloned()
                .collect()
        } else {
            ALL_POOLED_TOOLS
                .iter()
                .filter(|t| !VIRTUAL_EXCLUDED_TOOLS.contains(t))
                .map(|s| s.to_string())
                .collect()
        };

        // Permission config: follows main-agent configuration
        let skip_perms = self
            .get_skip_perms
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| f())
            .unwrap_or(true);

        // Bind permission bridge for virtual agents
        {
            let virtual_jid = format!("virtual:{task_id}");
            if let Some(ref bind_fn) = *self.on_bind_permission.lock().unwrap() {
                let cleanup = bind_fn(&virtual_jid, &persona.name, skip_perms);
                if let Some(cleanup) = cleanup {
                    *cleanup_guard.cleanup_permission.lock().unwrap() = Some(cleanup);
                }
            }
        }

        let timeout = timeout_override.unwrap_or(Duration::from_millis(DEFAULT_TIMEOUT_MS));

        // Execute via the core API trait
        let api = Arc::clone(&self.api);
        let temp_dir_str = temp_dir.to_string_lossy().to_string();
        let working_dir = workspace_dir.to_string();
        let instance_id = instance_id.to_string();
        let prompt = prompt.to_string();
        let tools_clone = tools.clone();
        let sys_prompt = persona.system_prompt.clone();
        let persona_name = persona.name.clone();
        let task_id_captured = task_id.to_string();
        let extra_mcp_servers = self.extra_mcp_servers.lock().unwrap().clone();
        let perm_fn = self.permission_fn.lock().unwrap().clone();
        let virtual_jid_for_perm = format!("virtual:{task_id}");

        // Wrap the pool's todos_notify to send with the correct virtual JID
        // and persona name.
        let todos_notify: Option<TodosNotifyFn> =
            self.todos_notify
                .lock()
                .unwrap()
                .clone()
                .map(move |notify| {
                    let virtual_jid = format!("virtual:{task_id_captured}");
                    Arc::new(
                        move |_instance_id: &str, _label: &str, todos: &[TodoItem]| {
                            notify(&virtual_jid, &persona_name, todos);
                        },
                    ) as TodosNotifyFn
                });

        let custom_memory_dir_captured = custom_memory_dir.map(|s| s.to_string());
        let memory_folder_override_captured = memory_folder_override.map(|s| s.to_string());

        // Wrap activity_notify with task_id for this run.
        let activity_notify: Option<ActivityNotifyFn> =
            self.activity_notify.lock().unwrap().clone().map(|notify| {
                let tid = task_id.to_string();
                Arc::new(move |_: &str, entry: SubAgentActivityEntry| {
                    notify(&tid, entry);
                }) as ActivityNotifyFn
            });

        let usage_accum_for_run = Arc::clone(&usage_accum);
        let result = tokio::task::spawn_blocking(move || {
            let sys_prompt_opt = if sys_prompt.is_empty() {
                None
            } else {
                Some(sys_prompt.as_str())
            };
            api.execute_virtual_prompt(
                &instance_id,
                &temp_dir_str,
                &working_dir,
                &tools_clone,
                sys_prompt_opt,
                &[], // skills_extra_dirs — not yet ported from TS
                skip_perms,
                &prompt,
                timeout,
                cancel,
                todos_notify,
                &extra_mcp_servers,
                &virtual_jid_for_perm,
                perm_fn,
                custom_memory_dir_captured.as_deref(),
                memory_folder_override_captured.as_deref(),
                activity_notify,
                Some(usage_accum_for_run),
            )
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(join_err) => {
                if join_err.is_cancelled() {
                    bail!("Cancelled");
                }
                bail!("Virtual agent failed: {join_err}");
            }
        }
    }

    fn decrement_count(&self, persona_name: &str) {
        let mut counts = self.active_counts.lock().unwrap();
        let entry = counts.get(persona_name).copied();
        if let Some(count) = entry {
            if count <= 1 {
                counts.remove(persona_name);
            } else {
                counts.insert(persona_name.to_string(), count - 1);
            }
        }
    }
}

// ===== CleanupGuard =====

/// Ensures cleanup runs even on early return / error paths.
struct CleanupGuard<'p> {
    pool: &'p VirtualWorkerPool,
    persona_name: String,
    task_id: String,
    temp_dir: PathBuf,
    cleanup_permission: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl CleanupGuard<'_> {
    fn run(&self) {
        // Remove running instance
        {
            let mut instances = self.pool.running_instances.lock().unwrap();
            if let Some(inst) = instances.get(&self.task_id) {
                if !inst.count_decremented_early {
                    self.pool.decrement_count(&self.persona_name);
                }
            }
            instances.remove(&self.task_id);
        }

        // Run permission cleanup
        if let Some(cleanup) = self.cleanup_permission.lock().unwrap().take() {
            cleanup();
        }

        // Remove temp dir
        if self.temp_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.temp_dir);
        }
    }
}

// ===== Helpers =====

/// Final messages shorter than this are suspected to be nudge echoes
/// ("No more tool calls...") rather than the actual deliverable.
const SHORT_FINAL_CHARS: usize = 200;

/// Pick the text returned as the virtual run's result. A stall-nudged model
/// often ends the session with a short junk echo of the nudge, while the real
/// deliverable was produced a few turns earlier — when the final message is
/// suspiciously short and a much longer message exists, return the longer one.
fn pick_result_text(
    final_text: String,
    longest_message: Option<String>,
    instance_id: &str,
) -> String {
    let final_len = final_text.trim().chars().count();
    if final_len >= SHORT_FINAL_CHARS {
        return final_text;
    }
    match longest_message {
        Some(longest)
            if longest.trim().chars().count() >= SHORT_FINAL_CHARS * 2
                && longest.trim().chars().count() > final_len * 3 =>
        {
            tracing::warn!(
                "[VirtualAgent:{instance_id}] final message is a short tail ({final_len} chars) — \
                 returning the longest message ({} chars) as the result instead",
                longest.trim().chars().count()
            );
            longest
        }
        _ => final_text,
    }
}

fn generate_instance_id(persona_name: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let ts36 = radix36(ts);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{persona_name}-{ts36}-{n}")
}

fn radix36(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

// ===== ZenVirtualCoreApi =====

/// Real [`VirtualCoreApi`] backed by ZenEngine.
///
/// Holds an optional shared [`McpManager`] so dispatched sub-agents see the same
/// external MCP servers as the main agent — in particular the Space-App MCP
/// servers auto-registered at `McpScopeType::Project`. Without it, a sub-agent
/// running a Space-App persona/skill would find that app's `mcp__<app>-mcp__*`
/// tools absent.
pub struct ZenVirtualCoreApi {
    mcp_manager: Option<Arc<McpManager>>,
}

impl ZenVirtualCoreApi {
    pub fn new(mcp_manager: Option<Arc<McpManager>>) -> Self {
        Self { mcp_manager }
    }
}

impl VirtualCoreApi for ZenVirtualCoreApi {
    #[allow(clippy::too_many_arguments)]
    fn execute_virtual_prompt(
        &self,
        instance_id: &str,
        agent_data_dir: &str,
        working_dir: &str,
        _tools: &[String],
        system_prompt: Option<&str>,
        _skills_extra_dirs: &[String],
        skip_perms: bool,
        prompt: &str,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
        todos_notify: Option<TodosNotifyFn>,
        extra_mcp_servers: &[McpServerConfig],
        virtual_jid: &str,
        permission_fn: Option<VirtualPermissionFn>,
        custom_memory_dir: Option<&str>,
        memory_folder_override: Option<&str>,
        activity_notify: Option<ActivityNotifyFn>,
        usage_accum: Option<Arc<UsageAccum>>,
    ) -> Result<String> {
        use crate::zen_core::{EngineEvent, SessionState, ZenCore, ZenCoreOptions};

        let handle = tokio::runtime::Handle::current();

        let opts = ZenCoreOptions {
            instance_id: instance_id.to_string(),
            working_dir: working_dir.to_string(),
            agent_data_dir: agent_data_dir.to_string(),
            skip_file_edit_permission: skip_perms,
            skip_bash_exec_permission: skip_perms,
            skip_skill_permission: skip_perms,
            skip_mcp_tool_permission: skip_perms,
            system_prompt: system_prompt
                .unwrap_or("You are a helpful AI assistant.")
                .to_string(),
            custom_memory_dir: custom_memory_dir.map(|s| s.to_string()),
            memory_folder_override: memory_folder_override.map(|s| s.to_string()),
            max_agent_turns: Some(virtual_max_turns()),
            ..Default::default()
        };

        let engine = crate::zen_core::ZenEngine::new(opts, self.mcp_manager.clone());
        engine.create_session(None)?;

        // Pull in the shared external MCP tools (Space-App servers registered at
        // Project scope + user MCP) so this sub-agent has parity with the main
        // agent. Must run BEFORE injecting the per-worker subprocess servers
        // below: refresh_mcp_tools() clears every `mcp__*` tool and re-adds only
        // the manager's, which would otherwise wipe the freshly-injected browser
        // tools.
        if self.mcp_manager.is_some() {
            handle.block_on(engine.refresh_mcp_tools());
        }

        // Inject extra MCP servers (e.g. browser-mcp) so virtual agents have browser tools.
        // Then wait briefly to allow the async spawn tasks to register their tools before
        // process_user_input queries the tool list.
        if !extra_mcp_servers.is_empty() {
            for mcp_cfg in extra_mcp_servers {
                if let Err(e) = engine.add_or_update_mcp_server(mcp_cfg, "virtual") {
                    tracing::warn!(
                        "[VirtualAgent:{instance_id}] Failed to inject MCP server '{}': {e}",
                        mcp_cfg.name
                    );
                }
            }
            // Give the tokio::spawn tasks inside add_or_update_mcp_server time to connect.
            handle.block_on(tokio::time::sleep(std::time::Duration::from_millis(500)));
        }

        let mut rx = engine.event_bus.subscribe();
        engine.process_user_input(prompt, None)?;

        let mut last_message: Option<String> = None;
        let mut longest_message: Option<String> = None;
        let deadline = std::time::Instant::now() + timeout;

        loop {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                engine.abort_current();
                bail!("Virtual agent cancelled");
            }
            if std::time::Instant::now() > deadline {
                engine.abort_current();
                bail!("Virtual agent timed out after {}s", timeout.as_secs());
            }

            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let result =
                handle.block_on(async { tokio::time::timeout(remaining, rx.recv()).await });

            match result {
                Ok(Ok(event)) => match event {
                    EngineEvent::MessageComplete(data) => {
                        if data.agent_id == crate::agent::agent_pool::MAIN_AGENT_ID {
                            // Forward the worker's reasoning (think) as its own
                            // activity entry. Many turns of a small model are
                            // think-only → tool-call (no user-visible content),
                            // and the old code skipped the whole block when
                            // content was empty, hiding the worker's trace from
                            // the inline DAG view. Emit think and message as
                            // separate entries so the chat-side activity feed
                            // shows the full step-by-step like the side panel.
                            if !data.reasoning.trim().is_empty() {
                                if let Some(ref notify) = activity_notify {
                                    notify(
                                        instance_id,
                                        SubAgentActivityEntry {
                                            entry_type: "think".into(),
                                            tool_name: None,
                                            title: None,
                                            summary: None,
                                            content: None,
                                            ok: None,
                                            text: Some(data.reasoning.clone()),
                                            ts: chrono::Utc::now().to_rfc3339(),
                                        },
                                    );
                                }
                            }
                            if !data.content.trim().is_empty() {
                                // Track the longest message separately: a model
                                // that gets stall-nudged often ends on a short
                                // junk echo ("No more tool calls...") after
                                // having already produced the real deliverable.
                                if data.content.trim().chars().count()
                                    > longest_message
                                        .as_deref()
                                        .map(|m: &str| m.trim().chars().count())
                                        .unwrap_or(0)
                                {
                                    longest_message = Some(data.content.clone());
                                }
                                last_message = Some(data.content.clone());
                                if let Some(ref notify) = activity_notify {
                                    notify(
                                        instance_id,
                                        SubAgentActivityEntry {
                                            entry_type: "message".into(),
                                            tool_name: None,
                                            title: None,
                                            summary: None,
                                            content: None,
                                            ok: None,
                                            text: Some(data.content),
                                            ts: chrono::Utc::now().to_rfc3339(),
                                        },
                                    );
                                }
                            }
                        }
                    }
                    EngineEvent::StateUpdate(data) => {
                        if data.state == SessionState::Idle {
                            let from_history = engine.last_main_assistant_visible_text();
                            let final_text = if !from_history.trim().is_empty() {
                                from_history
                            } else {
                                last_message.unwrap_or_default()
                            };
                            return Ok(pick_result_text(final_text, longest_message, instance_id));
                        }
                    }
                    EngineEvent::SessionError(data) => {
                        bail!("Virtual agent error: {}", data.error.message);
                    }
                    EngineEvent::TodosUpdate(items) => {
                        if let Some(ref notify) = todos_notify {
                            let todos: Vec<TodoItem> = items
                                .into_iter()
                                .map(|t| TodoItem {
                                    content: t.content,
                                    status: t.status,
                                    active_form: t.active_form,
                                })
                                .collect();
                            notify(instance_id, "virtual", &todos);
                        }
                    }
                    EngineEvent::ToolPermissionRequest(data) => {
                        if let Some(ref perm_fn) = permission_fn {
                            // Forward to admin UI via PermissionBridge and block
                            // until the user responds (or we time out).
                            let (tx, rx) = std::sync::mpsc::sync_channel::<String>(1);
                            perm_fn(
                                virtual_jid.to_string(),
                                data.tool_name.clone(),
                                data.permission_key.clone(),
                                data.title.clone(),
                                data.content.clone(),
                                data.options.clone(),
                                tx,
                            );
                            tracing::info!(
                                "[VirtualAgent:{instance_id}] permission forwarded to admin UI, waiting for response tool={}",
                                data.tool_name
                            );
                            let selected = rx
                                .recv_timeout(std::time::Duration::from_secs(600))
                                .unwrap_or_else(|_| {
                                    tracing::warn!(
                                        "[VirtualAgent:{instance_id}] permission response timed out; refusing tool={}",
                                        data.tool_name
                                    );
                                    "refuse".to_string()
                                });
                            engine.respond_to_tool_permission(
                                crate::zen_core::ToolPermissionResponseData {
                                    tool_name: data.tool_name,
                                    selected,
                                },
                            );
                        } else {
                            tracing::warn!(
                                "[VirtualAgent:{instance_id}] no permission handler wired; auto-refusing tool={}",
                                data.tool_name
                            );
                            engine.respond_to_tool_permission(
                                crate::zen_core::ToolPermissionResponseData {
                                    tool_name: data.tool_name,
                                    selected: "refuse".to_string(),
                                },
                            );
                        }
                    }
                    EngineEvent::ToolExecutionComplete(data) => {
                        if let Some(ref notify) = activity_notify {
                            notify(
                                instance_id,
                                SubAgentActivityEntry {
                                    entry_type: "tool".into(),
                                    tool_name: Some(data.tool_name),
                                    title: Some(data.title),
                                    summary: Some(data.summary),
                                    content: Some(data.content),
                                    ok: Some(true),
                                    text: None,
                                    ts: chrono::Utc::now().to_rfc3339(),
                                },
                            );
                        }
                    }
                    EngineEvent::ToolExecutionError(data) => {
                        if let Some(ref notify) = activity_notify {
                            notify(
                                instance_id,
                                SubAgentActivityEntry {
                                    entry_type: "tool".into(),
                                    tool_name: Some(data.tool_name),
                                    title: Some(data.title),
                                    summary: None,
                                    content: Some(serde_json::Value::String(data.content)),
                                    ok: Some(false),
                                    text: None,
                                    ts: chrono::Utc::now().to_rfc3339(),
                                },
                            );
                        }
                    }
                    // Token accounting: this engine is private to the run (no
                    // AgentPool bridge on its bus), so record here — and total
                    // up for the VirtualRunResult / agent.run reply.
                    EngineEvent::LlmUsage(d) => {
                        if let Some(rec) = crate::usage::global() {
                            rec.record(
                                crate::usage::UsageEvent {
                                    jid: virtual_jid.to_string(),
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
                        if let Some(ref accum) = usage_accum {
                            accum
                                .tokens_in
                                .fetch_add(d.usage.input(), std::sync::atomic::Ordering::Relaxed);
                            accum
                                .tokens_out
                                .fetch_add(d.usage.output(), std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    _ => {}
                },
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    bail!("Virtual agent event bus closed");
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                    tracing::warn!(
                        "[VirtualAgent:{instance_id}] event bus lagged (skipped {skipped}); will use transcript on idle if needed"
                    );
                }
                Err(_elapsed) => {
                    engine.abort_current();
                    bail!("Virtual agent timed out after {}s", timeout.as_secs());
                }
            }
        }
    }
}

// ===== Tests =====

#[cfg(test)]
mod tests {
    use super::*;

    struct StubVirtualCoreApi;

    impl VirtualCoreApi for StubVirtualCoreApi {}

    #[test]
    fn test_radix36() {
        assert_eq!(radix36(0), "0");
        assert_eq!(radix36(10), "a");
        assert_eq!(radix36(35), "z");
        assert_eq!(radix36(36), "10");
    }

    #[test]
    fn test_generate_instance_id() {
        let id1 = generate_instance_id("coder");
        let id2 = generate_instance_id("coder");
        assert!(id1.starts_with("coder-"));
        assert!(id2.starts_with("coder-"));
        assert_ne!(id1, id2, "instance IDs should be unique");
    }

    #[test]
    fn test_virtual_excluded_tools() {
        assert!(VIRTUAL_EXCLUDED_TOOLS.contains(&"Task"));
        assert!(VIRTUAL_EXCLUDED_TOOLS.contains(&"AskUserQuestion"));
    }

    #[test]
    fn test_get_active_count_default_zero() {
        let api = Arc::new(StubVirtualCoreApi);
        let pool = VirtualWorkerPool::new(api);
        assert_eq!(pool.get_active_count("nonexistent"), 0);
    }

    #[test]
    fn test_cancel_task_nonexistent_does_not_panic() {
        let api = Arc::new(StubVirtualCoreApi);
        let pool = VirtualWorkerPool::new(api);
        pool.cancel_task("nonexistent");
    }

    #[test]
    fn test_cancel_all_empty_does_not_panic() {
        let api = Arc::new(StubVirtualCoreApi);
        let pool = VirtualWorkerPool::new(api);
        pool.cancel_all();
    }
}

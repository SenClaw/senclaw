//! IsolatedRunner — one-shot disposable agent execution.
//!
//! Port of `code-old/SenClaw/src/agent/IsolatedRunner.ts`.
//!
//! Decoupled from AgentPool / GroupBinding / ScheduleTool. Use cases:
//!   1. AgentPool::run_isolated — scheduled tasks (caller wraps ScheduleTool MCP + broadcastReply)
//!   2. `senclaw agent-task` CLI — hook scripts (reflection, summarization, analysis)
//!
//! Behavior:
//!   - skip_mcp_init = true (avoid concurrent MCP race, matches AgentPool)
//!   - skip_*_permission default true (unattended)
//!   - Collects `MessageComplete` events for agent_id == "main"
//!   - Resolves on `StateUpdate(Idle)`
//!   - Forces resolve with `timed_out: true` on timeout (no error)

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::time::timeout as tokio_timeout;

use crate::zen_core::{
    AgentMode, EngineEvent, McpServerConfig, MessageCompleteData, SessionState, StateUpdateData,
    ZenCore, ZenCoreOptions, ZenEngine, MAIN_AGENT_ID,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// MCP server configuration registered before session creation.
#[derive(Debug, Clone)]
pub struct McpInject {
    pub config: McpServerConfig,
    pub scope: String,
}

/// Live-activity callback: `(kind, text)` where kind ∈
/// think | text | tool | tool_error | message. Lets callers (workflow steps)
/// surface what the isolated agent is doing in real time.
#[derive(Clone)]
pub struct OnActivity(pub Arc<dyn Fn(&str, &str) + Send + Sync>);

impl std::fmt::Debug for OnActivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnActivity(..)")
    }
}

/// Permission skip flags. All default to `true` for unattended one-shot runs.
#[derive(Debug, Clone)]
pub struct SkipPermissions {
    pub file_edit: bool,
    pub bash_exec: bool,
    pub skill: bool,
    pub mcp_tool: bool,
}

impl Default for SkipPermissions {
    fn default() -> Self {
        Self {
            file_edit: true,
            bash_exec: true,
            skill: true,
            mcp_tool: true,
        }
    }
}

/// One-shot agent execution options.
#[derive(Debug, Clone)]
pub struct OneShotOptions {
    /// User prompt (required).
    pub prompt: String,
    /// Working directory (file I/O, Bash, etc.).
    pub working_dir: String,
    /// Agent data dir (CLAUDE.md, .sema/). Defaults to `working_dir` when `None`.
    pub agent_data_dir: Option<String>,
    /// Multi-tenant instance key. Auto-generated when `None`.
    pub instance_id: Option<String>,
    /// Tool whitelist. Empty = all tools.
    pub use_tools: Vec<String>,
    /// Extra skills directories.
    pub skills_extra_dirs: Vec<String>,
    /// System prompt override.
    pub system_prompt: Option<String>,
    /// Custom user rules appended to system prompt.
    pub custom_rules: Option<String>,
    /// Agent mode.
    pub agent_mode: AgentMode,
    /// MCP servers registered before session creation.
    pub mcp_configs: Vec<McpInject>,
    /// Timeout (defaults to 5 minutes).
    pub timeout: Option<Duration>,
    /// Permission skip flags (all default true).
    pub skip_permissions: SkipPermissions,
    /// Cooperative cancellation: when cancelled, the engine is aborted and
    /// the result returns with `aborted: true` (used by workflow cancel).
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// Agent-loop turn budget override. `None` = engine default (30). Raise
    /// for browser-driven research sessions (~2 turns per page).
    pub max_agent_turns: Option<usize>,
    /// LLM config to run against. `None` = the globally active model.
    ///
    /// The chat path picks a model per session via `GroupBinding.llm_config_id`
    /// → `CoreApi::set_model_override`. One-shot runs had no equivalent, so
    /// every caller was pinned to whatever model happened to be active — fine
    /// for a CLI invocation, not for a background task that wants a cheap model
    /// for routine upkeep and a strong one for customer-facing work.
    pub model_config_id: Option<String>,
    /// Live-activity stream (thinking deltas, tool calls, messages).
    pub on_activity: Option<OnActivity>,
    /// Record this run as a trajectory under the given id.
    ///
    /// A one-shot has no chat, so it had no trajectory either — which left the
    /// evals harness scoring whatever a *real* chat last did, in whatever
    /// state that chat's workspace happened to be in. Naming an id here turns
    /// the run itself into a scorable turn under
    /// `~/.senclaw/trajectories/<id>/`. Recording is forced on for that id in
    /// this process only, never written to `enabled.json`.
    pub trajectory_jid: Option<String>,
}

impl Default for OneShotOptions {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            working_dir: String::new(),
            agent_data_dir: None,
            instance_id: None,
            use_tools: Vec::new(),
            skills_extra_dirs: Vec::new(),
            system_prompt: None,
            custom_rules: None,
            agent_mode: AgentMode::Agent,
            mcp_configs: Vec::new(),
            timeout: None,
            skip_permissions: SkipPermissions::default(),
            cancel: None,
            max_agent_turns: None,
            model_config_id: None,
            trajectory_jid: None,
            on_activity: None,
        }
    }
}

/// Result of one-shot execution.
#[derive(Debug, Clone)]
pub struct OneShotResult {
    /// Last `MessageComplete` content from agent_id == "main".
    pub text: String,
    /// All non-empty `MessageComplete` contents from agent_id == "main", in order.
    pub all_texts: Vec<String>,
    /// Wall-clock duration.
    pub duration: Duration,
    /// Number of `message:complete` events on the main agent.
    pub turn_count: u32,
    /// `true` if execution ended via timeout (engine forcibly aborted).
    pub timed_out: bool,
    /// `true` if execution ended via the `cancel` token (engine aborted).
    pub aborted: bool,
    /// `true` if the session surfaced a terminal error (api/model/context…).
    /// Mirrors the TS runner's fast-fail on `session:error` — without it a
    /// mid-step LLM failure ends the loop via Idle and the caller sees a
    /// "successful" run with an empty `text`.
    pub errored: bool,
    /// Description of the session error when `errored` (`<code>: <message>`).
    pub error_message: Option<String>,
    /// Total billed input/output tokens across the run's LLM calls (0 when
    /// the provider reported no usage). Per-call rows are in `llm_usage_log`;
    /// these totals let callers (background runner) store per-run numbers
    /// without racing the recorder's async flush.
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// Build a unique instance id like `oneshot-{millis}-{rand}` if caller didn't supply one.
fn gen_instance_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let rand: u32 = rand::random();
    format!("oneshot-{millis}-{:x}", rand & 0xFFFFF)
}

/// Record one engine event as a trajectory line when this run was given an
/// id, then hand it back. A pass-through so the event loop below keeps its
/// shape — the trajectory is a side channel, not a step in the run.
///
/// `cp_jid` is always present (the caller falls back to the instance id when
/// no `--trajectory` id was given) so the control-plane trace — on by default,
/// unlike the opt-in trajectory — still gets written for a one-shot run, which
/// is what lets `scripts/evals/run.py` read cache/decision stats back out.
fn trace(jid: Option<&str>, cp_jid: &str, event: EngineEvent) -> EngineEvent {
    if let Some(tj) = jid {
        crate::trajectory::record(tj, &event);
    }
    crate::control_plane::trace::record(cp_jid, &event);
    event
}

/// Run a single prompt to idle, then dispose. Mirrors TS `runOneShot`.
pub async fn run_one_shot(opts: OneShotOptions) -> Result<OneShotResult> {
    let started_at = Instant::now();
    let timeout = opts.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let instance_id = opts.instance_id.unwrap_or_else(gen_instance_id);
    let agent_data_dir = opts
        .agent_data_dir
        .clone()
        .unwrap_or_else(|| opts.working_dir.clone());

    let zen_opts = ZenCoreOptions {
        instance_id: instance_id.clone(),
        agent_data_dir,
        working_dir: opts.working_dir.clone(),
        use_tools: opts.use_tools.clone(),
        skills_extra_dirs: opts.skills_extra_dirs.clone(),
        skip_file_edit_permission: opts.skip_permissions.file_edit,
        skip_bash_exec_permission: opts.skip_permissions.bash_exec,
        skip_skill_permission: opts.skip_permissions.skill,
        skip_mcp_tool_permission: opts.skip_permissions.mcp_tool,
        skip_mcp_init: true,
        system_prompt: opts.system_prompt.clone().unwrap_or_default(),
        custom_rules: opts.custom_rules.clone().unwrap_or_default(),
        agent_mode: opts.agent_mode,
        max_agent_turns: opts.max_agent_turns,
        model_config_id: opts.model_config_id.clone(),
        ..Default::default()
    };

    // Recording is opt-in per run and process-local: naming an id here must
    // not switch recording on for a chat that shares it.
    let traj_jid = opts.trajectory_jid.clone();
    if let Some(tj) = &traj_jid {
        crate::trajectory::force_enable(tj);
    }
    // Control-plane trace jid: reuse the trajectory id when the caller gave
    // one (so `scripts/evals/run.py` can find both under the same name),
    // else the instance id — trace is on by default, so a one-shot run
    // always gets one regardless of `--trajectory`.
    let cp_jid = traj_jid.clone().unwrap_or_else(|| instance_id.clone());

    let engine = ZenEngine::new(zen_opts, None);

    let mut rx = engine.event_bus.subscribe();
    engine.create_session(Some(&format!("session-{instance_id}")))?;

    // Inject MCP servers AFTER create_session (session setup rebuilds tool
    // registries), then give the async connect tasks a beat to register their
    // tools before the prompt queries the tool list — mirrors
    // VirtualWorkerPool's ordering.
    if !opts.mcp_configs.is_empty() {
        for inject in &opts.mcp_configs {
            if let Err(e) = engine.add_or_update_mcp_server(&inject.config, &inject.scope) {
                tracing::warn!(
                    "[IsolatedRunner:{instance_id}] add_or_update_mcp_server '{}' failed: {e}",
                    inject.config.name
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Pre-discover whitelisted deferred tools: a persona that names MCP tools
    // (e.g. browser_navigate) expects them in the roster immediately. Without
    // this the defer filter hides them behind ToolSearch and small models
    // stall searching instead of working. Mirrors DAG-mode pre-discovery.
    if !opts.use_tools.is_empty() {
        let deferred = engine.deferred_tools();
        let mut pre_discovered = 0usize;
        let mut discovered = engine.discovered_tools.lock().unwrap();
        for entry in &opts.use_tools {
            if let Some(t) =
                crate::tools::tool_search::resolve_tool_by_name(entry, deferred.as_slice())
            {
                discovered.insert(t.name().to_string());
                pre_discovered += 1;
            }
        }
        if pre_discovered > 0 {
            tracing::info!(
                "[IsolatedRunner:{instance_id}] pre-discovered {pre_discovered} whitelisted deferred tool(s)"
            );
        }
    }

    engine.process_user_input(&opts.prompt, None)?;

    let mut all_texts: Vec<String> = Vec::new();
    let mut turn_count: u32 = 0;
    let mut timed_out = false;
    let mut aborted = false;
    let mut errored = false;
    let mut error_message: Option<String> = None;
    let mut tokens_in: u64 = 0;
    let mut tokens_out: u64 = 0;
    // Only accept Idle AFTER the session has been observed doing work.
    // create_session / plugin init can leave an early Idle StateUpdate in the
    // event channel; breaking on it would dispose the engine before the LLM
    // request is even sent ("Request cancelled before send") and return an
    // empty text. Mirrors the TS runner's `sawProcessing` guard.
    let mut saw_processing = false;
    let deadline = Instant::now() + timeout;
    let cancel = opts.cancel.clone().unwrap_or_default();

    if cancel.is_cancelled() {
        aborted = true;
        engine.abort_current();
    }

    while !aborted {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            timed_out = true;
            engine.abort_current();
            break;
        }

        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                aborted = true;
                engine.abort_current();
                break;
            }
            next = tokio_timeout(remaining, rx.recv()) => next,
        };

        match next {
            // Timed out waiting for next event
            Err(_) => {
                timed_out = true;
                engine.abort_current();
                break;
            }
            // Channel closed — treat as terminal.
            Ok(Err(_)) => break,
            Ok(Ok(event)) => match trace(traj_jid.as_deref(), &cp_jid, event) {
                EngineEvent::MessageComplete(MessageCompleteData {
                    agent_id, content, ..
                }) if agent_id == MAIN_AGENT_ID => {
                    turn_count += 1;
                    if !content.trim().is_empty() {
                        if let Some(cb) = &opts.on_activity {
                            (cb.0)("message", &content);
                        }
                        all_texts.push(content);
                    }
                }
                EngineEvent::ThinkingChunk(d) => {
                    if let Some(cb) = &opts.on_activity {
                        if !d.delta.is_empty() {
                            (cb.0)("think", &d.delta);
                        }
                    }
                }
                EngineEvent::TextChunk(d) => {
                    if let Some(cb) = &opts.on_activity {
                        if !d.delta.is_empty() {
                            (cb.0)("text", &d.delta);
                        }
                    }
                }
                EngineEvent::ToolExecutionComplete(d) => {
                    if let Some(cb) = &opts.on_activity {
                        let summary = if d.summary.trim().is_empty() {
                            d.title.clone()
                        } else {
                            d.summary.clone()
                        };
                        (cb.0)("tool", &format!("{} — {}", d.tool_name, summary));
                    }
                }
                EngineEvent::ToolExecutionError(d) => {
                    if let Some(cb) = &opts.on_activity {
                        (cb.0)("tool_error", &format!("{}: {}", d.tool_name, d.content));
                    }
                }
                EngineEvent::StateUpdate(StateUpdateData { state }) => {
                    if state == SessionState::Idle {
                        if saw_processing {
                            break;
                        }
                    } else {
                        saw_processing = true;
                    }
                }
                EngineEvent::SessionError(err) => {
                    tracing::warn!(
                        "[IsolatedRunner:{instance_id}] session error: {} ({})",
                        err.error.message,
                        err.error.code
                    );
                    // Fast-fail (TS parity): session errors are terminal
                    // (api/model/context) — waiting for Idle just converts
                    // them into a silent empty result.
                    errored = true;
                    error_message = Some(format!("{}: {}", err.error.code, err.error.message));
                    break;
                }
                // Token accounting — private engine, no AgentPool bridge on
                // this bus, so the runner records its own calls.
                EngineEvent::LlmUsage(d) => {
                    tokens_in += d.usage.input();
                    tokens_out += d.usage.output();
                    if let Some(rec) = crate::usage::global() {
                        rec.record(
                            crate::usage::UsageEvent {
                                jid: format!("isolated:{instance_id}"),
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
                }
                _ => {}
            },
        }
    }

    // Fire-and-forget dispose (engine handles SessionEnd hooks internally).
    {
        let engine: Arc<ZenEngine> = engine;
        tokio::task::spawn_blocking(move || engine.dispose());
    }

    // Prefer the engine's last-visible-text helper if available; otherwise
    // fall back to the last collected MessageComplete.
    let text = all_texts.last().cloned().unwrap_or_default();

    Ok(OneShotResult {
        text,
        all_texts,
        duration: started_at.elapsed(),
        turn_count,
        timed_out,
        aborted,
        errored,
        error_message,
        tokens_in,
        tokens_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_skip_permissions_all_true() {
        let s = SkipPermissions::default();
        assert!(s.file_edit && s.bash_exec && s.skill && s.mcp_tool);
    }

    #[test]
    fn gen_instance_id_unique() {
        let a = gen_instance_id();
        let b = gen_instance_id();
        assert_ne!(a, b);
        assert!(a.starts_with("oneshot-"));
    }

    #[test]
    fn one_shot_options_default_has_5min_timeout_fallback() {
        let opts = OneShotOptions::default();
        let t = opts.timeout.unwrap_or(DEFAULT_TIMEOUT);
        assert_eq!(t, Duration::from_secs(300));
    }
}

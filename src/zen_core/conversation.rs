//! Core query loop — the heart of the agent engine.
//!
//! Flow:
//! ```text
//! messages + system_prompt
//!   → query_llm() → assistant message
//!   → emit message:complete + conversation:usage
//!   → if no tool calls → finalize, return
//!   → run_tools() (concurrent or serial)
//!   → handle abort checkpoints
//!   → handle control signal rebuild (mode switch)
//!   → on context_length_exceeded → compact → retry (once)
//!   → recurse
//! ```
//!
//! Port of TS `Conversation.ts`.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use reqwest::Client;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::hooks::{ErrorInput, HookEvent, HookInput, HookInputBase, HookManager, PreCompactInput};
use super::*;
use crate::control_plane::context_assembler::{self, AgentStatusInput};
use crate::zen_core::events::ResponseRegistry;
use crate::zen_core::query_llm;
use crate::zen_core::run_tools::{self, PermissionChecker, RunContext};

/// Interruption message inserted when the session is aborted.
pub const INTERRUPT_MESSAGE: &str =
    "Session was interrupted. The current operation has been cancelled.";

/// Number of most-recent messages to keep during auto-compaction.
const COMPACT_KEEP_RECENT: usize = 12;

/// Fallback context window when a model profile reports no `context_length`.
/// Mirrors the TS default in `util/tokens.ts` / `util/compact.ts`.
const DEFAULT_CONTEXT_LENGTH: u64 = 128_000;

/// Trigger proactive auto-compaction once input tokens reach this fraction of
/// the model context window. Mirrors TS `AUTO_COMPACT_THRESHOLD_RATIO`.
const AUTO_COMPACT_THRESHOLD_RATIO: f64 = 0.75;
/// Minimum message count before the **after-process** stage will proactively
/// compact a completed conversation. Below this the history is small enough that
/// summarizing it would cost an LLM call without meaningful benefit (and risk
/// losing recent detail), so `compact_now` is a no-op.
const AFTER_PROCESS_MIN_MESSAGES: usize = 16;
/// Hard cap for a single LLM request turn (cloud/API providers).
/// Dispatch tasks have their own larger timeout, but the model call itself
/// should not be able to hang silently with no message/tool events.
const LLM_TURN_TIMEOUT: Duration = Duration::from_secs(180);

/// Timeout for local native inference (Candle CPU/Metal **and** MLX native).
///
/// CPU inference is O(L²) in sequence length and un-batched; even a small model
/// (0.6B) on a debug build can take several minutes for a short response.
/// Release builds are typically 8–15× faster, so this is intentionally generous.
/// MLX native with TurboQuant KV quantization (`kv_cache_bits` set) is similarly
/// slow on long prefill — TQ4 quantizes ~12 K tokens × 36 layers on CPU when the
/// prompt holds many MCP tool schemas, and easily exceeds the cloud-API timeout.
///
/// Hard breakdown for `local-candle-native`:
///   • prefill  ≤ 512 tokens × 28 layers → ~50 s (debug) / ~5 s (release)
///   • decode   ≤ 512 tokens              → ~50 s (debug) / ~5 s (release)
///   • total (debug)   ≈ 100–180 s
///   • total (release) ≈ 10–20 s
const LLM_TURN_TIMEOUT_LOCAL: Duration = Duration::from_secs(900);

/// Compact message history when context length is exceeded.
/// Keeps the first user message (original task framing) and the most recent messages,
/// dropping middle messages to reduce token count.
fn compact_messages(messages: &mut Vec<Message>) -> bool {
    if messages.len() <= COMPACT_KEEP_RECENT + 2 {
        return false;
    }
    let first_user_idx = messages.iter().position(|m| m.msg_type == "user");
    let keep_from = messages.len().saturating_sub(COMPACT_KEEP_RECENT);

    let mut kept: Vec<Message> = Vec::new();
    if let Some(idx) = first_user_idx {
        if idx < keep_from {
            kept.push(messages[idx].clone());
            kept.push(create_user_message(vec![ContentBlock::Text {
                text: "[Context compacted — earlier messages were dropped to stay within the \
                       token limit. The conversation continues with the most recent context preserved.]"
                    .into(),
            }]));
        }
    }
    kept.extend(messages.drain(keep_from..));
    *messages = kept;
    true
}

/// Prompt used to ask the main model for a lossless session snapshot.
/// Ported verbatim from TS `prompt/compact.ts` `COMPRESSION_PROMPT`.
const COMPRESSION_PROMPT: &str = "Create a lossless state snapshot of this session so any later instance can seamlessly resume work.

Cover the following (merge sections freely, but omit nothing):

A. **Intent evolution** — User requests in time order, how they changed, final shape. Include key user messages verbatim.
B. **Technical context** — Frameworks, toolchains, architecture, runtime environment.
C. **Artifacts & changes** — Files examined/modified/created. Embed full source for key changes.
D. **Errors & fixes** — All anomalies, fix paths, and user corrections.
E. **Open items** — Closed vs in-progress vs remaining work, with blockers.
F. **Interruption point** — Exact files, functions, edit actions at the moment of interruption.
G. **Continuation path** (only if applicable) — Quote user's follow-up intent, task name, suggested handoff.

## Rules
- Archive only from conversation content — no speculation or fabrication.
- Label gaps as \"not confirmed in context\".
- No tool calls — pure text reasoning and archival.
- Prefer full source over vague description.
";

const COMPACT_NOTICE: &str = "[Context Compression Notice]
The conversation has been automatically compressed due to token limit. Below is a comprehensive summary.";

/// Fire a PreCompact/PostCompact hook (non-blocking, spawned). No-op when no
/// hook manager or no hooks registered for the event.
fn spawn_compact_hook(config: &QueryConfig, messages: &[Message], event: HookEvent) {
    let Some(hm) = config.hook_manager.clone() else {
        return;
    };
    if !hm.has_hooks_for_event(&event) {
        return;
    }
    let base = HookInputBase {
        hook_event_name: event.clone(),
        session_id: config.session_id.clone(),
        agent_id: config.agent_id.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        cwd: config.working_dir.clone(),
    };
    let context_history: Vec<serde_json::Value> = messages
        .iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .collect();
    let hook_input = HookInput::PreCompact(PreCompactInput {
        base,
        message_count: messages.len(),
        context_history,
    });
    let (client, profile) = (config.hook_client.clone(), config.hook_profile.clone());
    let usage_bus = config.event_bus.clone();
    tokio::spawn(async move {
        let _ = super::hooks::execute_hooks(
            &hm,
            &event,
            &hook_input,
            &super::hooks::ExecuteHooksOptions {
                env: std::collections::HashMap::new(),
                cancel: None,
                client: client.as_ref(),
                profile: profile.as_ref(),
                messages: None,
                usage_bus: Some(&usage_bus),
            },
        )
        .await;
    });
}

/// Index of the last "real" user message (not a tool_result carrier). History
/// before this index can be safely compacted while the current turn is kept
/// intact, guaranteeing the message list still ends on a user message and that
/// tool_use/tool_result pairs are not split. Mirrors TS `autoCompact`.
fn last_real_user_index(messages: &[Message]) -> Option<usize> {
    messages.iter().rposition(|m| {
        m.msg_type == "user"
            && !matches!(
                m.message.content.first(),
                Some(ContentBlock::ToolResult { .. })
            )
    })
}

/// The text of the user input this whole `query()` call is working on —
/// captured once, before the loop's first LLM call, from the last "real"
/// (non-tool-result) user message. Used only for `<agent_status>`'s "original
/// goal" field; never re-derived mid-loop, so it stays the actual original
/// request even after compaction summarizes everything before it.
fn goal_text(messages: &[Message]) -> String {
    let Some(idx) = last_real_user_index(messages) else {
        return String::new();
    };
    messages[idx]
        .message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The message list actually sent to the LLM for one call.
///
/// Identical to `messages` — the same borrow, not even a clone — when
/// `agent_status` is off: the request must be byte-for-byte what it was
/// before this feature existed. When on, a full clone with one
/// `<agent_status>` text block appended to the *last user-role* message
/// (where tool results already live, per the internal message format — see
/// `docs/control-plane.md` §4). `messages` itself is never touched, so the
/// persisted history this function's caller eventually returns, the
/// trajectory recorder and compaction's input are all unaffected regardless
/// of this switch.
fn outgoing_messages<'a>(messages: &'a [Message], agent_status_on: bool, input: &AgentStatusInput) -> Cow<'a, [Message]> {
    if !agent_status_on {
        return Cow::Borrowed(messages);
    }
    // No user-role message to attach to should not happen (the loop always
    // ends on one), but degrading to "unchanged" is safer than guessing a
    // different role to inject into.
    let Some(idx) = messages.iter().rposition(|m| m.message.role == "user") else {
        return Cow::Borrowed(messages);
    };
    let mut clone = messages.to_vec();
    clone[idx].message.content.push(ContentBlock::Text {
        text: context_assembler::render(input),
    });
    Cow::Owned(clone)
}

/// Build the post-compaction history: a compression notice (user) followed by
/// the model-generated summary (assistant). The summary message carries a
/// corrected usage reflecting only notice + summary, so the token gauge drops
/// immediately. Mirrors TS `executeAutoCompact`. Deliberately does not go
/// through [`outgoing_messages`] — `<agent_status>` must never reach
/// compaction input.
async fn summarize_history(
    history: &[Message],
    config: &QueryConfig,
    cancel: &CancellationToken,
) -> Result<(Vec<Message>, String)> {
    let mut msgs = history.to_vec();
    msgs.push(create_user_message(vec![ContentBlock::Text {
        text: COMPRESSION_PROMPT.to_string(),
    }]));

    let compact_started = std::time::Instant::now();
    let summary_msg = query_llm::query_llm(
        &config.http_client,
        &msgs,
        "An AI assistant that helps summarize coding conversations.",
        &[],
        cancel,
        &config.profile,
        false,
        false,
        None, // compaction is internal — nothing to stream to the UI
    )
    .await?;

    // Record the REAL cost of the compaction call before it gets replaced by
    // the fabricated gauge value below — a full-history summarization is often
    // the most expensive single call in a session and must not vanish from
    // accounting.
    if let Some(u) = summary_msg.usage.as_ref().filter(|u| !u.is_empty()) {
        config.event_bus.emit(EngineEvent::LlmUsage(LlmUsageData {
            source: "compact".to_string(),
            agent_id: config.agent_id.clone(),
            session_id: config.session_id.clone(),
            profile: config.profile.name.clone(),
            provider: config.profile.provider.clone(),
            model: config.profile.model_name.clone(),
            usage: u.clone(),
            latency_ms: compact_started.elapsed().as_millis() as u64,
            ok: true,
        }));
    }

    let (summary_text, _reasoning, _tools) = extract_content(&summary_msg);
    if summary_text.trim().is_empty() {
        anyhow::bail!("compaction produced an empty summary");
    }

    let notice = create_user_message(vec![ContentBlock::Text {
        text: COMPACT_NOTICE.to_string(),
    }]);

    // Correct usage: post-compaction context ≈ notice (~30 tokens) + summary.
    let summary_tokens = summary_msg.usage.as_ref().map(|u| u.output()).unwrap_or(0);
    let corrected_usage = if summary_tokens > 0 {
        Some(RawUsage {
            input_tokens: Some(30 + summary_tokens),
            output_tokens: Some(summary_tokens),
            ..Default::default()
        })
    } else {
        None
    };

    let summary_assistant = Message {
        msg_type: "assistant".to_string(),
        message: MessagePayload {
            role: "assistant".to_string(),
            content: vec![ContentBlock::Text {
                text: summary_text.clone(),
            }],
        },
        uuid: uuid::Uuid::new_v4().to_string(),
        usage: corrected_usage,
    };

    Ok((vec![notice, summary_assistant], summary_text))
}

/// Result of an auto-compaction attempt.
struct CompactOutcome {
    messages: Vec<Message>,
    exec: CompactExecData,
    /// Usage to broadcast after compaction (reflects the compacted history).
    usage: UsageData,
    /// True when compaction actually changed the message list.
    changed: bool,
}

/// **After-process stage.** Run *after* a turn completes (not in the query
/// loop) to keep the stored conversation compact and coherent — the same
/// Claude-Code-style LLM summarization as the in-loop safety compaction, but
/// triggered eagerly once the history has grown past
/// [`AFTER_PROCESS_MIN_MESSAGES`] rather than only at the 75% context threshold.
///
/// Earlier messages are summarized into a lossless snapshot while the most
/// recent turn is preserved verbatim; on summary failure it falls back to
/// keep-recent truncation (inherited from [`auto_compact`]). Emits the same
/// Compact/Usage events + Pre/PostCompact hooks as the in-loop path so the UI
/// reflects the compaction. Returns the (possibly unchanged) message list;
/// no-op for subagents and trivially short conversations.
pub async fn compact_now(
    mut messages: Vec<Message>,
    config: &QueryConfig,
    cancel: &CancellationToken,
) -> Vec<Message> {
    if config.is_subagent || messages.len() < AFTER_PROCESS_MIN_MESSAGES {
        return messages;
    }
    info!(
        agent_id = %config.agent_id,
        msg_count = messages.len(),
        "after-process: proactively compacting completed conversation"
    );
    config
        .event_bus
        .emit(EngineEvent::CompactStart(CompactStartData {
            message_count: messages.len(),
        }));
    spawn_compact_hook(config, &messages, HookEvent::PreCompact);

    let outcome = auto_compact(std::mem::take(&mut messages), config, cancel).await;
    messages = outcome.messages;

    config
        .event_bus
        .emit(EngineEvent::CompactExec(outcome.exec));
    spawn_compact_hook(config, &messages, HookEvent::PostCompact);
    if outcome.changed {
        config
            .event_bus
            .emit(EngineEvent::ConversationUsage(ConversationUsageData {
                usage: outcome.usage,
            }));
        info!(
            agent_id = %config.agent_id,
            new_msg_count = messages.len(),
            "after-process: conversation compacted"
        );
    }
    messages
}

/// Proactively compact the conversation via LLM summarization, preserving the
/// current turn. Falls back to deterministic keep-recent truncation if the
/// summary call fails. Mirrors TS `autoCompact` + `compactMessages`.
async fn auto_compact(
    messages: Vec<Message>,
    config: &QueryConfig,
    cancel: &CancellationToken,
) -> CompactOutcome {
    let ctx = config.profile.context_length;
    let token_before = count_tokens(&messages, ctx).use_tokens;

    let Some(idx) = last_real_user_index(&messages) else {
        return CompactOutcome {
            usage: count_tokens(&messages, ctx),
            exec: CompactExecData {
                err_msg: Some("no user message to anchor compaction".into()),
                token_before,
                token_compact: token_before,
                compact_rate: 1.0,
                summary: None,
            },
            messages,
            changed: false,
        };
    };

    let history = &messages[..idx];
    let keep = messages[idx..].to_vec();

    if history.len() < 2 {
        return CompactOutcome {
            usage: count_tokens(&messages, ctx),
            exec: CompactExecData {
                err_msg: Some("history too small to compact".into()),
                token_before,
                token_compact: token_before,
                compact_rate: 1.0,
                summary: None,
            },
            messages,
            changed: false,
        };
    }

    let (compacted_history, summary, err_msg) =
        match summarize_history(history, config, cancel).await {
            Ok((compacted, summary)) => (compacted, Some(summary), None),
            Err(e) => {
                // Fallback: deterministic keep-recent truncation over the full list.
                warn!(
                    agent_id = %config.agent_id,
                    error = %e,
                    "LLM compaction failed — falling back to keep-recent truncation"
                );
                let mut truncated = messages.clone();
                let did = compact_messages(&mut truncated);
                if !did {
                    return CompactOutcome {
                        usage: count_tokens(&messages, ctx),
                        exec: CompactExecData {
                            err_msg: Some(format!("compaction failed: {e}")),
                            token_before,
                            token_compact: token_before,
                            compact_rate: 1.0,
                            summary: None,
                        },
                        messages,
                        changed: false,
                    };
                }
                // For the truncation fallback the whole list is the result.
                let usage = count_tokens(&truncated, ctx);
                return CompactOutcome {
                    exec: CompactExecData {
                        err_msg: None,
                        token_before,
                        token_compact: usage.use_tokens,
                        compact_rate: if token_before > 0 {
                            usage.use_tokens as f64 / token_before as f64
                        } else {
                            1.0
                        },
                        summary: None,
                    },
                    usage,
                    messages: truncated,
                    changed: true,
                };
            }
        };

    // Usage after compaction reflects only the compacted history (notice +
    // summary); the kept current turn re-counts on the next real LLM turn.
    let usage_after = count_tokens(&compacted_history, ctx);
    let mut final_messages = compacted_history;
    final_messages.extend(keep);

    CompactOutcome {
        exec: CompactExecData {
            err_msg,
            token_before,
            token_compact: usage_after.use_tokens,
            compact_rate: if token_before > 0 {
                usage_after.use_tokens as f64 / token_before as f64
            } else {
                1.0
            },
            summary,
        },
        usage: usage_after,
        messages: final_messages,
        changed: true,
    }
}

/// Configuration passed to the query loop. All fields are owned so the config
/// can be moved into spawned tasks.
/// Resolver returning the current tool list. Called once per turn so newly
/// `ToolSearch`-discovered tools become available without recreating the
/// engine. Mirrors the same pattern used by `TaskTool::tools_resolver`.
pub type ToolsResolver = Arc<dyn Fn() -> Vec<Arc<dyn Tool>> + Send + Sync>;

pub struct QueryConfig {
    pub agent_id: String,
    pub working_dir: String,
    pub agent_data_dir: String,
    pub system_prompt: String,
    /// Closure returning the current tool list. Re-invoked each conversation
    /// turn so `ToolSearch` discoveries flow into subsequent turns within
    /// the same user input (the loop is one user message → many LLM turns).
    pub tools: ToolsResolver,
    pub http_client: Client,
    pub event_bus: EventBus,
    pub response_registry: Option<Arc<ResponseRegistry>>,
    pub permission_checker: Arc<dyn PermissionChecker>,
    pub profile: ModelProfile,
    pub thinking: bool,
    pub stream: bool,
    pub is_subagent: bool,
    /// Optional hook manager for PreToolUse/PostToolUse hooks.
    pub hook_manager: Option<Arc<HookManager>>,
    pub hook_client: Option<Client>,
    pub hook_profile: Option<ModelProfile>,
    pub session_id: String,
    /// Enable prompt caching (Anthropic only — cache_control on system + last tool).
    pub enable_cache: bool,
    /// `controlPlane.agentStatus` (§7 status bar) — append `<agent_status>` to
    /// a per-call CLONE of the outgoing messages, never to `messages` itself.
    /// Not affected by `SENCLAW_JEV_OFF`: this is code-computed, not a Jev
    /// call. See [`outgoing_messages`].
    pub agent_status: bool,
    /// Per-engine turn-budget override (see [`max_agent_turns`]). `None` = default.
    pub max_turns_override: Option<usize>,
    /// Current agent mode (Agent / Plan / Dag). Drives `task_done` enforcement:
    /// Plan and Dag modes nudge the model toward `task_done` when it stops with
    /// text only. Agent mode keeps legacy "text = done" behavior so ordinary
    /// chat is unaffected.
    pub agent_mode: AgentMode,
    /// Source of user inputs that queued up while this turn runs (main agent
    /// only; `None` for subagents). Drained after each tool round and appended
    /// to the last tool result so the model sees mid-turn user messages
    /// without restarting the loop (mirrors TS
    /// `injectPendingInputsIntoToolResult`).
    pub pending_inject: Option<PendingInjectSource>,
}

/// Closure draining queued inject-type inputs (stops before the next
/// `/command`). See [`StateManager::consume_inject_inputs_before_next_command`].
pub type PendingInjectSource = Arc<dyn Fn() -> Vec<PendingUserInput> + Send + Sync>;

/// Append queued mid-turn user inputs to the current tool-result blocks as
/// `<system-reminder>` texts. Returns how many were injected.
fn inject_pending_inputs(blocks: &mut Vec<ContentBlock>, config: &QueryConfig) -> usize {
    if config.is_subagent {
        return 0;
    }
    let Some(source) = &config.pending_inject else {
        return 0;
    };
    let items = source();
    for item in &items {
        blocks.push(ContentBlock::Text {
            text: format!(
                "<system-reminder>\nNew user message arrived during your task:\n{}\n\n\
                 Reminder: Once current task finishes, immediately respond to the user's \
                 latest message. Do not skip it.\n</system-reminder>",
                item.input
            ),
        });
    }
    if !items.is_empty() {
        info!(
            "[{}] injected {} queued user input(s) into tool results",
            config.agent_id,
            items.len()
        );
    }
    items.len()
}

/// Absolute cap on LLM turns per user input — a backstop against runaway
/// agentic loops (small models can call tools forever). Resolution order:
/// `SENCLAW_MAX_AGENT_TURNS` env (explicit user intent) → per-engine override
/// (e.g. virtual workers doing browser research get a higher budget) → 30.
fn max_agent_turns(override_turns: Option<usize>) -> usize {
    std::env::var("SENCLAW_MAX_AGENT_TURNS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .or(override_turns)
        .unwrap_or(30)
}

/// After this many *consecutive* turns that call the same tool(s) and produce
/// no user-facing text, nudge the model to stop and answer. Override with
/// `SENCLAW_STALL_TOOL_TURNS`. The hard stop is twice this value.
fn stall_tool_turns() -> usize {
    std::env::var("SENCLAW_STALL_TOOL_TURNS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(4)
}

/// Instruction injected when the agent is detected spinning on the same tool.
const STALL_NUDGE: &str = "You have called the same tool several times in a row \
without producing an answer. You already have enough information. Stop calling tools now \
and write your final answer to the user, in the user's language, based on what you have gathered.";

/// Instruction injected when the turn budget (or stall hard-stop) is reached.
/// The next LLM call runs with tools disabled, so this is the model's one
/// chance to salvage the work it has done so far into a real answer.
const FINAL_ANSWER_NUDGE: &str = "Your tool budget for this task is exhausted and tool access \
is now disabled. Based on everything you have gathered above, write your complete final answer \
to the user now, in the user's language. Include the concrete findings, data, and sources you \
collected. Do not mention the tool budget, do not apologize, and do not describe what you would \
have done — just deliver the best answer possible from the information above.";

/// Instruction injected when a tool has failed several times in a row with the
/// same error. Gives the model one clear chance to self-correct (fix the
/// argument the error names) or stop honestly — before the hard stop below.
const TOOL_ERROR_NUDGE: &str = "The tool you just called has now failed several times in a row \
with the same error (see the error results above). Stop repeating the identical call. Either \
(a) fix the specific problem the error describes — for example supply the missing or incorrect \
argument it names — and try once more, or (b) if you cannot fix it, stop calling tools and tell \
the user plainly, in their language, that the operation failed, quoting the error. Do NOT \
fabricate, estimate, or guess any result you were unable to obtain.";

/// Instruction injected when a repeated tool-error loop is force-stopped. Unlike
/// `FINAL_ANSWER_NUDGE` this must NOT invite the model to synthesize findings —
/// there are none; the whole point is that the tool never succeeded — so it
/// explicitly forbids inventing data and asks for an honest failure report.
const TOOL_ERROR_FINAL_NUDGE: &str = "A tool you depended on kept returning errors (see the \
repeated error results above) and tool access is now disabled. You could NOT obtain the \
information this task required. Do not invent, estimate, or guess any results. Reply to the user \
in their language: state clearly which operation failed, quote the error message, and say the \
task could not be completed. Do not add anything you did not actually obtain.";

/// Max retries for silent empty completions (LLM returned 200 OK with 0 blocks
/// and 0 tool calls — observed with MLX 4-bit / qwen3.5-4b-optiq under heavy
/// tool counts). Each retry backs off 500ms × attempt. Override with
/// `SENCLAW_EMPTY_RETRIES`. Set to 0 to disable retries.
fn max_empty_retries() -> u8 {
    std::env::var("SENCLAW_EMPTY_RETRIES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(2)
}

/// Max times to nudge the model with a "call task_done or continue next tool"
/// prompt when the conversation looks like a multi-step task and the model
/// replied with text only. Cap protects against text↔nudge ping-pong on weak
/// models. Override with `SENCLAW_COMPLETION_NUDGES`. Set to 0 to disable.
fn max_completion_nudges() -> u8 {
    std::env::var("SENCLAW_COMPLETION_NUDGES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(2)
}

/// Each mode has a different "natural completion" signal. A text-only response
/// without that signal is a premature stop — `completion_status()` returns the
/// nudge to inject for that mode. The three modes:
///
/// - `Agent`: single conversational loop. Text = done. Nothing to nudge.
/// - `Plan` (set by `EnterPlanMode`): single agent researches, writes a plan
///   file under `<plans_dir>/<title>.md`, then calls `ExitPlanMode(plan=...)`
///   for user approval. `ExitPlanMode` is the canonical exit; nothing else
///   counts as plan-mode completion. See `prompt::plan_mode_reminder`.
/// - `Dag` (set by `update_agent_mode(Dag)`): orchestrator. Researches with
///   read-only tools, designs a task graph, then calls
///   `DispatchCreateParentAndRun` (which BLOCKS until every sub-agent in the
///   DAG completes). Workers spawned by the dispatch bridge themselves run in
///   plain `AgentMode::Agent`, not `Dag`. After dispatch returns, the
///   orchestrator's text report IS the natural completion — so post-dispatch
///   text-only is fine. Pre-dispatch text-only is a premature stop.
///
/// `task_done` is accepted as a generic completion signal in any mode (handled
/// by the short-circuit further below), but the mode-specific tool is always
/// the preferred exit.
/// The mode that should drive completion logic this turn. A model can switch
/// into Plan mode mid-turn via `EnterPlanMode`, which flips the live engine
/// mode but not `config.agent_mode` (captured at turn start) — so once that
/// tool has run, treat an Agent-mode turn as Plan for nudging / `task_done`.
fn effective_agent_mode(base: AgentMode, entered_plan_mode: bool) -> AgentMode {
    if entered_plan_mode && matches!(base, AgentMode::Agent) {
        AgentMode::Plan
    } else {
        base
    }
}

/// Whether the DAG orchestrator *lead* may call `tool`. The lead researches and
/// dispatches; it never mutates the workspace directly (workers do that in Agent
/// mode with the full tool set). Keeping the lead's surface small is also what
/// keeps the request under endpoint tool-count limits — the browser MCP alone is
/// ~30 tools and is pure worker territory.
///
/// Kept: read-only builtins (Read/Grep/Glob/ToolSearch/Time…), the dispatch and
/// memory MCP servers, and a short allowlist of non-read-only orchestration
/// builtins the lead genuinely needs. Everything else — Write/Edit/Bash, the
/// browser/wiki/send/workspace/schedule MCP surfaces — is withheld.
fn is_dag_lead_tool(tool: &dyn Tool) -> bool {
    let name = tool.name();
    // Read-only research tools (Read, Grep, Glob, ToolSearch, Time, …).
    if tool.is_read_only() {
        return true;
    }
    // Dispatch (the lead's whole purpose) and memory recall for research. Match
    // both the canonical `mcp__senclaw-<server>__` and the "senclaw-"-stripped
    // bridge form so the filter is robust to either naming scheme.
    if name.starts_with("mcp__senclaw-dispatch__")
        || name.starts_with("mcp__dispatch__")
        || name.starts_with("mcp__senclaw-memory__")
        || name.starts_with("mcp__memory__")
    {
        return true;
    }
    // Non-read-only builtins the lead needs to orchestrate, plan, ask, and
    // report. `DispatchCreateParent`/`DispatchCreateParentAndRun` are the builtin
    // dispatch tools; the rest are interaction / planning / completion.
    const ORCH_ALLOW: &[&str] = &[
        "DispatchCreateParent",
        "DispatchCreateParentAndRun",
        "task_done",
        "TodoWrite",
        "Task",
        "AskUser",
        "AskUserQuestion",
        "FormUI",
        "EnterPlanMode",
        "ExitPlanMode",
        "PersonaUpdate",
        "Skill",
        "WebFetch",
    ];
    ORCH_ALLOW.contains(&name)
}

fn completion_nudge_for(
    mode: AgentMode,
    exit_plan_mode_called: bool,
    dispatch_executed: bool,
    nudge_count: u8,
    nudge_cap: u8,
) -> Option<String> {
    let header = format!(" (Nudge {nudge_count}/{nudge_cap})");
    match mode {
        AgentMode::Agent => None,
        AgentMode::Plan => {
            if exit_plan_mode_called {
                // ExitPlanMode already ran this session — a follow-up
                // text-only turn is a legitimate wrap-up after planning.
                None
            } else {
                Some(format!(
                    "You must finish by calling `ExitPlanMode(plan=\"...\")` to request approval \
                     — a text-only reply does not satisfy this. If your plan file is ready, call \
                     `ExitPlanMode` now. If the plan needs more work, call the next research tool \
                     (Read/Grep/Glob/Task). To abandon entirely, call `task_done(summary=\"...\")`. \
                     Do NOT stop with text only — there is no user to type 'continue'.{header}"
                ))
            }
        }
        AgentMode::Dag => {
            if dispatch_executed {
                // Dispatch already ran (and returned, since the orchestrator
                // is back in the loop). The text-only reply is the orchestrator
                // reporting results to the user — that's the natural completion
                // path per the orchestration reminder. Don't nudge.
                None
            } else {
                Some(format!(
                    "You must orchestrate via the dispatch tools — text alone does not delegate \
                     any work. If you have enough context, call `DispatchCreateParentAndRun` with \
                     the task graph (it blocks until every sub-agent finishes). If you still need \
                     to research, call a read-only tool (Read/Grep/Glob). If the work is \
                     genuinely trivial and needs no dispatch, call `task_done(summary=\"...\")`. \
                     Do NOT stop with text only.{header}"
                ))
            }
        }
    }
}

/// Tool names that, when called by the model, satisfy a mode's natural
/// completion. Used to track per-session whether the model has reached the
/// mode's "checkpoint" yet — see `completion_nudge_for`.
const EXIT_PLAN_MODE_TOOL: &str = "ExitPlanMode";
/// The tool a model calls to switch INTO Plan mode mid-turn. When this fires,
/// the live engine mode becomes Plan but `config.agent_mode` (captured at turn
/// start) stays Agent — so we track it here to upgrade the *effective* mode and
/// keep the plan-mode completion nudge active for the rest of the session.
const ENTER_PLAN_MODE_TOOL: &str = "EnterPlanMode";
/// Dispatch tools whose execution counts as "DAG orchestration started". Tools
/// like `DispatchListAgents` and bare `DispatchCreateParent` (without Run) are
/// informational and do not actually delegate work, so they are excluded.
const EXECUTING_DISPATCH_TOOLS: &[&str] = &[
    "DispatchCreateParentAndRun",
    "DispatchAllTasks",
    "DispatchTask",
];

/// Sorted, deduped set of tool names in a batch — a coarse signature used to
/// detect the model re-invoking the same tool turn after turn (args may vary
/// slightly, e.g. a tweaked search query, so names alone are the stable signal).
fn tool_names_sig(tool_uses: &[ContentBlock]) -> String {
    let mut names: Vec<&str> = tool_uses
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names.join(",")
}

/// Exact signature of one tool call: name + canonical JSON args. Used to catch
/// a model re-issuing an *identical* call (same query, same URL) that can only
/// return what it already has.
fn tool_call_sig(block: &ContentBlock) -> Option<String> {
    if let ContentBlock::ToolUse { name, input, .. } = block {
        Some(format!(
            "{name}\u{1}{}",
            serde_json::to_string(input).unwrap_or_default()
        ))
    } else {
        None
    }
}

/// Synthetic tool result returned in place of re-executing an identical call.
const DUPLICATE_CALL_NOTE: &str = "Duplicate call: you already called this tool with these exact \
arguments earlier in this conversation and received a result above. Re-running it returns nothing \
new. Use the information you already have to write your final answer to the user now. If you truly \
need different information, change the arguments — do not repeat the identical call.";

/// Tools whose re-invocation with identical args is legitimate — blocking
/// waits / status polls whose result changes as daemon-side state advances
/// (e.g. re-waiting on a dispatch parent after it left the queue). Exempt
/// from the duplicate-call interception.
const DUPLICATE_EXEMPT_TOOLS: &[&str] = &["DispatchAllTasks", "DispatchTask", "DispatchListAgents"];

fn is_duplicate_exempt(block: &ContentBlock) -> bool {
    matches!(block, ContentBlock::ToolUse { name, .. }
        if DUPLICATE_EXEMPT_TOOLS.contains(&name.as_str()))
}

/// True when every freshly-executed tool call this turn came back as an error.
/// Only the results belonging to `fresh` (actually-run) calls are considered —
/// synthetic duplicate-interception notes (`is_error: false`) are ignored, and
/// a turn with no fresh calls returns false. Used by the error-loop guard so a
/// tool that keeps failing (e.g. `ssh_start_connect` → "Missing host, port, or
/// user") is caught regardless of whether the model narrates text alongside it.
fn fresh_results_all_errored(results: &[ContentBlock], fresh: &[ContentBlock]) -> bool {
    let fresh_ids: std::collections::HashSet<&str> = fresh
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    if fresh_ids.is_empty() {
        return false;
    }
    let mut saw = false;
    for r in results {
        if let ContentBlock::ToolResult {
            tool_use_id,
            is_error,
            ..
        } = r
        {
            if fresh_ids.contains(tool_use_id.as_str()) {
                saw = true;
                if !*is_error {
                    return false;
                }
            }
        }
    }
    saw
}

/// Run the conversation query loop. Returns the final message history.
///
/// This is an async generator conceptually — each "turn" is a call to the LLM
/// followed by optional tool execution. Since Rust doesn't have native async
/// generators yet, we process all turns in a single future and emit events
/// along the way.
pub async fn query(
    mut messages: Vec<Message>,
    config: &QueryConfig,
    cancel: &CancellationToken,
) -> Result<Vec<Message>> {
    let mut compacted = false;
    let mut proactively_compacted = false;
    // `<agent_status>` inputs that do not change turn to turn: captured once
    // so a mid-loop compaction (which may summarize everything before the
    // current turn) cannot alter what "original goal" or "elapsed" mean.
    let query_started_at = Instant::now();
    let original_goal = goal_text(&messages);

    // Loop-guard state (see `max_agent_turns` / `stall_tool_turns`).
    let max_turns = max_agent_turns(config.max_turns_override);
    let stall_limit = stall_tool_turns();
    let mut turn: usize = 0;
    let mut last_sig: Option<String> = None;
    let mut stall_streak: usize = 0;
    let mut nudged = false;
    // Error-loop guard: consecutive turns whose freshly-executed tool calls ALL
    // returned an error for the same tool(s). Tracked independently of the
    // text-based stall above because a model that narrates a sentence every turn
    // while re-calling a failing tool resets `stall_streak` and would otherwise
    // spin until the `max_turns` backstop — then salvage the loop into a
    // *fabricated* final answer. This guard nudges early and, if unresolved,
    // stops with a reported error instead of a made-up success.
    let mut last_error_sig: Option<String> = None;
    let mut error_streak: usize = 0;
    let mut error_nudged = false;
    // Set when the turn budget (or stall hard-stop) is exhausted: the next LLM
    // call runs with tools disabled and its text reply is accepted as the final
    // answer — salvaging the gathered context instead of erroring out with
    // nothing.
    let mut forcing_final = false;
    // Exact (name+args) tool-call signatures already executed this user input,
    // so identical re-issues can be short-circuited instead of re-run.
    let mut seen_tool_sigs: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Resilience: retry transient EMPTY_COMPLETION before failing the session.
    let max_empty_retries_cap = max_empty_retries();
    let mut empty_retries: u8 = 0;
    // Mode-specific completion tracking. Plain Agent mode keeps the legacy
    // "text = done" behavior; Plan tracks `ExitPlanMode`, Dag tracks dispatch.
    // See `completion_nudge_for` for the per-mode semantics.
    let max_completion_nudges_cap = max_completion_nudges();
    let mut completion_nudges: u8 = 0;
    let mut exit_plan_mode_called: bool = false;
    let mut dispatch_executed: bool = false;
    // Set once the model calls `EnterPlanMode` mid-turn. Upgrades the effective
    // mode Agent→Plan so a later text-only reply is nudged toward `ExitPlanMode`
    // (which raises the approval dialog) instead of silently ending the turn.
    let mut entered_plan_mode: bool = false;

    loop {
        // Check cancellation before each LLM call
        if cancel.is_cancelled() {
            info!("[{}] query loop cancelled before LLM call", config.agent_id);
            return Ok(messages);
        }

        // Backstop: hard cap on turns per user input so a model that ignores
        // every nudge still terminates instead of looping until the user pauses.
        // Reaching the cap does NOT throw the gathered context away: one final
        // LLM call runs with tools disabled to turn it into a real answer.
        turn += 1;
        if turn > max_turns && !forcing_final {
            warn!(
                "[{}] agent loop limit reached ({} turns) — forcing a final answer (tools disabled)",
                config.agent_id, max_turns
            );
            forcing_final = true;
            messages.push(create_user_message(vec![ContentBlock::Text {
                text: FINAL_ANSWER_NUDGE.to_string(),
            }]));
        } else if turn > max_turns + 1 {
            // The forced-final turn also failed to produce a clean answer.
            warn!(
                "[{}] agent loop limit reached and forced-final turn failed — stopping",
                config.agent_id
            );
            config
                .event_bus
                .emit(EngineEvent::SessionError(SessionErrorData {
                    error_type: "agent_loop_limit".to_string(),
                    error: SessionErrorDetail {
                        code: "AGENT_LOOP_LIMIT".to_string(),
                        message: format!(
                            "Stopped after {max_turns} tool-calling turns without a final answer \
                             (likely a loop). Increase SENCLAW_MAX_AGENT_TURNS to allow more."
                        ),
                        details: None,
                    },
                }));
            return Ok(messages);
        }

        // 0. Proactive compaction: if the context is approaching the model
        //    limit, summarize the history *before* the next call so we don't
        //    hit a hard context_length error. Runs at most once per user input
        //    (the reactive path below remains as a safety net). Subagents skip.
        if !config.is_subagent
            && !proactively_compacted
            && needs_auto_compact(&messages, config.profile.context_length)
        {
            info!(
                agent_id = %config.agent_id,
                msg_count = messages.len(),
                "Context near limit — proactively compacting"
            );
            config
                .event_bus
                .emit(EngineEvent::CompactStart(CompactStartData {
                    message_count: messages.len(),
                }));
            spawn_compact_hook(config, &messages, HookEvent::PreCompact);

            let outcome = auto_compact(std::mem::take(&mut messages), config, cancel).await;
            messages = outcome.messages;
            proactively_compacted = true;

            config
                .event_bus
                .emit(EngineEvent::CompactExec(outcome.exec));
            spawn_compact_hook(config, &messages, HookEvent::PostCompact);
            if outcome.changed {
                config
                    .event_bus
                    .emit(EngineEvent::ConversationUsage(ConversationUsageData {
                        usage: outcome.usage,
                    }));
                info!(
                    agent_id = %config.agent_id,
                    new_msg_count = messages.len(),
                    "Proactive compaction complete"
                );
            }
        }

        // 1. Call the LLM. Resolve tools fresh each turn so any tool the
        //    model discovered via `ToolSearch` earlier in this conversation
        //    is included starting from the very next turn. The forced-final
        //    turn runs without tools so the model can only answer.
        let turn_tools: Vec<Arc<dyn Tool>> = if forcing_final {
            Vec::new()
        } else if matches!(config.agent_mode, AgentMode::Dag) {
            // The DAG orchestrator lead researches with read-only tools and
            // delegates ALL mutating work (writing code, running commands,
            // browsing) to dispatched workers (which run in Agent mode with the
            // full tool set). Handing the lead the entire ~70-tool surface — 30
            // of them browser tools — is not just semantically wrong; on
            // tool-count-limited endpoints (observed with `ag/gemini-pro-agent`)
            // it makes the model return an empty completion, surfacing as
            // EMPTY_COMPLETION. Prune to the orchestration essentials.
            (config.tools)()
                .into_iter()
                .filter(|t| is_dag_lead_tool(t.as_ref()))
                .collect()
        } else {
            (config.tools)()
        };
        info!(
            "[{}] LLM turn start: messages={} tools={} stream={}",
            config.agent_id,
            messages.len(),
            turn_tools.len(),
            config.stream
        );
        let llm_started = std::time::Instant::now();
        // Live text sink: every visible delta becomes a `TextChunk` event while
        // the turn runs, so the UI paints as the model writes and the voice
        // pipeline can speak the first sentence long before the turn ends.
        // Without this the whole answer only appears at `MessageComplete`.
        let delta_sink: query_llm::TextDeltaSink = {
            let bus = config.event_bus.clone();
            let agent_id = config.agent_id.clone();
            let cumulative = std::sync::Mutex::new(String::new());
            Arc::new(move |delta: &str| {
                let content = {
                    let mut c = cumulative.lock().unwrap();
                    c.push_str(delta);
                    c.clone()
                };
                bus.emit(EngineEvent::TextChunk(TextChunkData {
                    agent_id: agent_id.clone(),
                    content,
                    delta: delta.to_string(),
                }));
            })
        };
        // `<agent_status>` (§7): a per-call clone with the status appended to
        // the last user-role message — never `messages` itself. See
        // `outgoing_messages`'s docs for why this is the only seam that needs
        // to change, and `docs/control-plane.md` §4 for why it is safe to
        // wire live (code-computed, no web/tool content, bounded length).
        let status_input = AgentStatusInput {
            // Todo state and pending background events are not reachable at
            // this seam yet (they live in `StateManager`/the dispatch bridge,
            // neither wired into `QueryConfig`) — left at 0 rather than
            // threading new plumbing through for a status line that already
            // degrades gracefully to "no todo/no pending events known".
            todo_open: 0,
            todo_total: 0,
            call_index: turn as u32,
            call_budget: max_turns as u32,
            elapsed_secs: query_started_at.elapsed().as_secs(),
            pending_events: 0,
            original_goal: original_goal.clone(),
        };
        let outgoing = outgoing_messages(&messages, config.agent_status, &status_input);
        let llm_call = query_llm::query_llm(
            &config.http_client,
            &outgoing,
            &config.system_prompt,
            &turn_tools,
            cancel,
            &config.profile,
            config.thinking,
            config.stream,
            Some(&delta_sink),
        );
        // Local native inference (Candle CPU/Metal, MLX native) runs in-process
        // and is much slower than cloud APIs — especially MLX with TurboQuant
        // KV (TQ4 prefill on long prompts blows past 180 s easily).
        //
        // Space-App providers (`provider == "app:<id>"`) are the same class:
        // a local engine behind the daemon's own loopback proxy. A 26B
        // SSD-streaming model's first agent turn (dozens of MCP tool schemas)
        // exceeds the cloud budget in prefill alone, exactly like MLX above.
        let adapt = config.profile.adapt.as_deref().unwrap_or("");
        let turn_timeout = if adapt.starts_with("local-")
            || config.profile.provider.starts_with("app:")
        {
            LLM_TURN_TIMEOUT_LOCAL
        } else {
            LLM_TURN_TIMEOUT
        };
        let assistant_msg = match tokio::time::timeout(turn_timeout, llm_call).await {
            Err(_) => {
                let msg = format!(
                    "LLM request timed out after {}s without completing a turn",
                    turn_timeout.as_secs()
                );
                config
                    .event_bus
                    .emit(EngineEvent::SessionError(SessionErrorData {
                        error_type: "error".into(),
                        error: SessionErrorDetail {
                            code: "LLM_TIMEOUT".into(),
                            message: msg.clone(),
                            details: None,
                        },
                    }));

                // Fire Error hook
                if let Some(ref hm) = config.hook_manager {
                    if hm.has_hooks_for_event(&HookEvent::Error) {
                        let base = HookInputBase {
                            hook_event_name: HookEvent::Error,
                            session_id: config.session_id.clone(),
                            agent_id: config.agent_id.clone(),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            cwd: config.working_dir.clone(),
                        };
                        let hook_input = HookInput::Error(ErrorInput {
                            base,
                            error_message: msg.clone(),
                            error_type: Some("LLM_TIMEOUT".to_string()),
                        });
                        let (client, profile) =
                            (config.hook_client.clone(), config.hook_profile.clone());
                        let hm_clone = hm.clone();
                        let usage_bus = config.event_bus.clone();
                        tokio::spawn(async move {
                            let _ = super::hooks::execute_hooks(
                                &hm_clone,
                                &HookEvent::Error,
                                &hook_input,
                                &super::hooks::ExecuteHooksOptions {
                                    env: std::collections::HashMap::new(),
                                    cancel: None,
                                    client: client.as_ref(),
                                    profile: profile.as_ref(),
                                    messages: None,
                                    usage_bus: Some(&usage_bus),
                                },
                            )
                            .await;
                        });
                    }
                }
                return Err(anyhow::anyhow!(msg));
            }
            Ok(Ok(msg)) => {
                // Accounting funnel: one event per LLM call with real usage.
                // Subagent turns included (the UI-facing events are filtered
                // by agent_id downstream; this one must not be).
                if let Some(u) = msg.usage.as_ref().filter(|u| !u.is_empty()) {
                    config.event_bus.emit(EngineEvent::LlmUsage(LlmUsageData {
                        source: if config.is_subagent { "subagent" } else { "agent" }
                            .to_string(),
                        agent_id: config.agent_id.clone(),
                        session_id: config.session_id.clone(),
                        profile: config.profile.name.clone(),
                        provider: config.profile.provider.clone(),
                        model: config.profile.model_name.clone(),
                        usage: u.clone(),
                        latency_ms: llm_started.elapsed().as_millis() as u64,
                        ok: true,
                    }));
                }
                msg
            }
            Ok(Err(e)) => {
                let classified = query_llm::LlmError::classify(&e);
                if classified.should_emit() {
                    let error_data = classified.to_session_error();
                    config
                        .event_bus
                        .emit(EngineEvent::SessionError(error_data.clone()));

                    // Fire Error hook
                    if let Some(ref hm) = config.hook_manager {
                        if hm.has_hooks_for_event(&HookEvent::Error) {
                            let base = HookInputBase {
                                hook_event_name: HookEvent::Error,
                                session_id: config.session_id.clone(),
                                agent_id: config.agent_id.clone(),
                                timestamp: chrono::Utc::now().to_rfc3339(),
                                cwd: config.working_dir.clone(),
                            };
                            let hook_input = HookInput::Error(ErrorInput {
                                base,
                                error_message: error_data.error.message.clone(),
                                error_type: Some(error_data.error.code.clone()),
                            });
                            let (client, profile) =
                                (config.hook_client.clone(), config.hook_profile.clone());
                            let hm_clone = hm.clone();
                            let usage_bus = config.event_bus.clone();
                            tokio::spawn(async move {
                                let _ = super::hooks::execute_hooks(
                                    &hm_clone,
                                    &HookEvent::Error,
                                    &hook_input,
                                    &super::hooks::ExecuteHooksOptions {
                                        env: std::collections::HashMap::new(),
                                        cancel: None,
                                        client: client.as_ref(),
                                        profile: profile.as_ref(),
                                        messages: None,
                                        usage_bus: Some(&usage_bus),
                                    },
                                )
                                .await;
                            });
                        }
                    }
                }
                if classified.is_context_length && !compacted {
                    warn!(
                        agent_id = %config.agent_id,
                        msg_count = messages.len(),
                        "Context length exceeded — auto-compacting"
                    );
                    config
                        .event_bus
                        .emit(EngineEvent::CompactStart(CompactStartData {
                            message_count: messages.len(),
                        }));
                    spawn_compact_hook(config, &messages, HookEvent::PreCompact);

                    // Reactive path: the API already rejected the request as
                    // too large, so a summarization LLM call would likely fail
                    // too. Use deterministic keep-recent truncation, but report
                    // real before/after token metrics.
                    let token_before =
                        count_tokens(&messages, config.profile.context_length).use_tokens;
                    let did_compact = compact_messages(&mut messages);
                    let usage_after = count_tokens(&messages, config.profile.context_length);
                    config
                        .event_bus
                        .emit(EngineEvent::CompactExec(CompactExecData {
                            err_msg: if did_compact {
                                None
                            } else {
                                Some("compaction had no effect".into())
                            },
                            token_before,
                            token_compact: usage_after.use_tokens,
                            compact_rate: if token_before > 0 {
                                usage_after.use_tokens as f64 / token_before as f64
                            } else {
                                1.0
                            },
                            summary: None,
                        }));
                    spawn_compact_hook(config, &messages, HookEvent::PostCompact);

                    if did_compact {
                        info!(
                            agent_id = %config.agent_id,
                            new_msg_count = messages.len(),
                            "Auto-compact complete, retrying"
                        );
                        if !config.is_subagent {
                            config.event_bus.emit(EngineEvent::ConversationUsage(
                                ConversationUsageData { usage: usage_after },
                            ));
                        }
                        compacted = true;
                        continue;
                    }
                    warn!(
                        agent_id = %config.agent_id,
                        "Auto-compact had no effect, giving up"
                    );
                }
                return Err(e);
            }
        };

        // Checkpoint 1: after LLM response, before tool execution
        if cancel.is_cancelled() {
            info!("[{}] cancelled after LLM response", config.agent_id);
            let pending_tools = assistant_msg
                .message
                .content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::ToolUse { id, .. } = b {
                        Some(create_tool_result_stop(id))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            let mut interrupt_content: Vec<ContentBlock> = pending_tools;
            interrupt_content.push(ContentBlock::Text {
                text: INTERRUPT_MESSAGE.to_string(),
            });
            messages.push(assistant_msg);
            messages.push(create_user_message(interrupt_content));

            config
                .event_bus
                .emit(EngineEvent::SessionInterrupted(SessionInterruptedData {
                    agent_id: config.agent_id.to_string(),
                    content: INTERRUPT_MESSAGE.to_string(),
                }));
            return Ok(messages);
        }

        // 2. Emit message:complete
        let (text_content, reasoning, tool_uses) = extract_content(&assistant_msg);

        let has_tool_calls = !tool_uses.is_empty();
        info!(
            "[{}] LLM turn complete: text_len={} reasoning_len={} tool_calls={}",
            config.agent_id,
            text_content.len(),
            reasoning.len(),
            tool_uses.len()
        );

        // Guard against silent empty completions. Some custom OpenAI-compat
        // endpoints (observed with `qwen3.5-4b-optiq` and MLX 4-bit under
        // heavy tool counts) return 200 OK with zero blocks and zero tool
        // calls — model didn't generate anything. This is often transient
        // (quant-pressure / stop-token edge case), so retry with backoff a few
        // times before surfacing as a session error. The empty assistant
        // message is dropped from history either way to avoid poisoning the
        // next turn.
        if text_content.trim().is_empty() && reasoning.trim().is_empty() && tool_uses.is_empty() {
            if empty_retries < max_empty_retries_cap {
                empty_retries += 1;
                let backoff_ms = 500u64 * empty_retries as u64;
                warn!(
                    "[{}] empty LLM completion — retry {}/{} in {}ms",
                    config.agent_id, empty_retries, max_empty_retries_cap, backoff_ms
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                continue;
            }
            warn!(
                "[{}] empty LLM completion persists after {} retries — surfacing as session error.",
                config.agent_id, max_empty_retries_cap
            );
            config.event_bus.emit(EngineEvent::SessionError(
                crate::zen_core::SessionErrorData {
                    error_type: "empty_completion".to_string(),
                    error: crate::zen_core::SessionErrorDetail {
                        code: "EMPTY_COMPLETION".to_string(),
                        message: format!(
                            "LLM returned empty response after {} retries (no text / reasoning / tool calls). \
                             Check endpoint logs — common causes: auth failure, model overload, \
                             tool count exceeds endpoint limit.",
                            max_empty_retries_cap
                        ),
                        details: None,
                    },
                },
            ));
            return Ok(messages);
        }
        // Got a non-empty turn — reset the empty-completion retry budget so
        // a future hiccup later in the same session gets its own fresh allowance.
        empty_retries = 0;
        let tool_call_infos: Option<Vec<ToolCallInfo>> = if has_tool_calls {
            Some(
                tool_uses
                    .iter()
                    .filter_map(|b| {
                        if let ContentBlock::ToolUse { name, input, .. } = b {
                            Some(ToolCallInfo {
                                name: name.clone(),
                                args: input.clone(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect(),
            )
        } else {
            None
        };

        config
            .event_bus
            .emit(EngineEvent::MessageComplete(MessageCompleteData {
                agent_id: config.agent_id.to_string(),
                reasoning,
                content: text_content.clone(),
                has_tool_calls: has_tool_calls,
                tool_calls: tool_call_infos,
                // Prefer real API usage; fall back to a char-based estimate of
                // THIS message's output for providers that don't report usage
                // (e.g. local MLX/Candle inference), so the per-message token
                // badge still shows a meaningful number.
                output_tokens: assistant_msg
                    .usage
                    .as_ref()
                    .map(|u| u.output() as u32)
                    .filter(|&t| t > 0)
                    .unwrap_or_else(|| {
                        estimate_tokens_by_chars(std::slice::from_ref(&assistant_msg)) as u32
                    }),
            }));

        // 3. Emit conversation:usage (subagents skip this)
        if !config.is_subagent {
            let updated = {
                let mut msgs = messages.clone();
                msgs.push(assistant_msg.clone());
                msgs
            };
            let usage = count_tokens(&updated, config.profile.context_length);
            config
                .event_bus
                .emit(EngineEvent::ConversationUsage(ConversationUsageData {
                    usage,
                }));
        }

        // 4. No tools → maybe done. Per-mode handling:
        //    - Agent: text = done (always).
        //    - Plan: text is done iff `ExitPlanMode` already ran this session,
        //      otherwise nudge toward it (a text-only reply doesn't exit Plan
        //      mode per the plan_mode_reminder).
        //    - Dag: text is done iff a real dispatch tool already ran, otherwise
        //      nudge toward it (text without delegating any work isn't progress).
        //    Capped at `max_completion_nudges_cap` to avoid text↔nudge loops.
        // Effective mode upgrades Agent→Plan once `EnterPlanMode` ran this turn,
        // so a mid-turn switch into Plan mode still triggers the plan nudge.
        let effective_mode = effective_agent_mode(config.agent_mode, entered_plan_mode);
        if tool_uses.is_empty() {
            // Forced-final turn: accept the text as-is — mode-completion
            // nudging would just push the model back into a loop it can no
            // longer act on (tools are disabled).
            if let Some(nudge) = (!forcing_final)
                .then(|| {
                    completion_nudge_for(
                        effective_mode,
                        exit_plan_mode_called,
                        dispatch_executed,
                        completion_nudges + 1,
                        max_completion_nudges_cap,
                    )
                })
                .flatten()
            {
                if completion_nudges < max_completion_nudges_cap {
                    completion_nudges += 1;
                    info!(
                        "[{}] text-only response in {} mode without natural completion signal — \
                         nudging ({}/{})",
                        config.agent_id,
                        effective_mode.as_str(),
                        completion_nudges,
                        max_completion_nudges_cap
                    );
                    messages.push(assistant_msg);
                    messages.push(create_user_message(vec![ContentBlock::Text {
                        text: nudge,
                    }]));
                    continue;
                }
            }
            messages.push(assistant_msg);
            info!(
                "[{}] query complete — no tool calls, {} messages",
                config.agent_id,
                messages.len()
            );
            return Ok(messages);
        }

        // Forced-final turn but the model still emitted tool calls (possible
        // with local models that hallucinate tools) — give up cleanly rather
        // than looping; the transcript keeps whatever text it produced.
        if forcing_final {
            warn!(
                "[{}] forced-final turn still attempted {} tool call(s) — stopping",
                config.agent_id,
                tool_uses.len()
            );
            messages.push(assistant_msg);
            config
                .event_bus
                .emit(EngineEvent::SessionError(SessionErrorData {
                    error_type: "agent_loop_limit".to_string(),
                    error: SessionErrorDetail {
                        code: "AGENT_LOOP_LIMIT".to_string(),
                        message: format!(
                            "Stopped after {max_turns} tool-calling turns without a final answer \
                             (likely a loop). Increase SENCLAW_MAX_AGENT_TURNS to allow more."
                        ),
                        details: None,
                    },
                }));
            return Ok(messages);
        }

        // Per-mode completion tracking: note which mode-defining tools the
        // model called this turn so the "no tools" branch above can tell
        // whether a future text-only reply is the natural end of the mode.
        for tu in &tool_uses {
            if let ContentBlock::ToolUse { name, .. } = tu {
                if name == EXIT_PLAN_MODE_TOOL {
                    exit_plan_mode_called = true;
                } else if name == ENTER_PLAN_MODE_TOOL {
                    entered_plan_mode = true;
                } else if EXECUTING_DISPATCH_TOOLS.contains(&name.as_str()) {
                    dispatch_executed = true;
                }
            }
        }

        // 5. Run tools
        let ctx = RunContext {
            agent_id: &config.agent_id,
            working_dir: &config.working_dir,
            agent_data_dir: &config.agent_data_dir,
            tools: &turn_tools,
            tools_resolver: Some(config.tools.as_ref()),
            fire: &|event| config.event_bus.emit(event),
            permission_checker: config.permission_checker.as_ref(),
            event_bus: Some(&config.event_bus),
            response_registry: config.response_registry.as_deref(),
            hook_manager: config.hook_manager.clone(),
            hook_client: config.hook_client.clone(),
            hook_profile: config.hook_profile.clone(),
            session_id: config.session_id.clone(),
        };

        // Intercept exact-duplicate tool calls. A weak model often re-issues
        // the same call (identical search query, identical Skill load) without
        // ever using the result. Execute only fresh calls; for duplicates,
        // synthesize a result telling the model the data is already available.
        let mut fresh: Vec<ContentBlock> = Vec::new();
        let mut dup_ids: Vec<String> = Vec::new();
        for tu in &tool_uses {
            let id = match tu {
                ContentBlock::ToolUse { id, .. } => id.clone(),
                _ => continue,
            };
            if is_duplicate_exempt(tu) {
                // Blocking waits / status polls: identical args legitimately
                // return new data as daemon-side state advances.
                fresh.push(tu.clone());
                continue;
            }
            match tool_call_sig(tu) {
                Some(sig) if seen_tool_sigs.contains(&sig) => dup_ids.push(id),
                Some(sig) => {
                    seen_tool_sigs.insert(sig);
                    fresh.push(tu.clone());
                }
                None => fresh.push(tu.clone()),
            }
        }

        let mut tool_results = if fresh.is_empty() {
            Vec::new()
        } else {
            run_tools::run_tools(&fresh, cancel, &ctx).await
        };

        // Check for control signals (e.g. clearContextAndStart)
        let mut clear_context_payload = None;
        for b in &tool_results {
            if let ContentBlock::ControlSignal {
                signal_type,
                payload,
            } = b
            {
                if signal_type == "ClearContextAndStart" {
                    clear_context_payload = Some(payload.clone());
                }
            }
        }
        if let Some(payload) = clear_context_payload {
            let plan_content = payload
                .get("plan_content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            info!(
                "[{}] received ClearContextAndStart control signal — clearing history",
                config.agent_id
            );
            messages.clear();
            messages.push(create_user_message(vec![ContentBlock::Text {
                text: format!("按照以下计划进行实现：\n\n{}", plan_content),
            }]));
            // Trigger mode switch if needed (this matches TS behaviour)
            // The ZenEngine will handle PlanImplement event and set agent_mode
            // wait, but the event wasn't emitted here? Actually, the UI emits PlanExitResponse to engine,
            // which flips mode. But we can emit a PlanImplement event just in case, or just continue.
            continue;
        }

        if !dup_ids.is_empty() {
            info!(
                "[{}] intercepted {} duplicate tool call(s) — returning 'already have this' note",
                config.agent_id,
                dup_ids.len()
            );
            for id in &dup_ids {
                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: DUPLICATE_CALL_NOTE.to_string(),
                    is_error: false,
                });
            }
        }

        // Checkpoint 2: after tool execution, before recursion
        if cancel.is_cancelled() {
            info!("[{}] cancelled after tool execution", config.agent_id);

            // Generate stop messages for any incomplete tools
            let completed_ids: std::collections::HashSet<String> = tool_results
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                        Some(tool_use_id.clone())
                    } else {
                        None
                    }
                })
                .collect();

            let mut all_results = tool_results;
            for tu in &tool_uses {
                if let ContentBlock::ToolUse { id, .. } = tu {
                    if !completed_ids.contains(id) {
                        all_results.push(create_tool_result_stop(id));
                    }
                }
            }
            // Append interrupt text to last result
            if let Some(last) = all_results.last_mut() {
                if let ContentBlock::ToolResult { content, .. } = last {
                    content.push_str(&format!("\n\n{INTERRUPT_MESSAGE}"));
                }
            }

            messages.push(assistant_msg);
            if !all_results.is_empty() {
                messages.push(create_user_message(all_results));
            }

            config
                .event_bus
                .emit(EngineEvent::SessionInterrupted(SessionInterruptedData {
                    agent_id: config.agent_id.to_string(),
                    content: INTERRUPT_MESSAGE.to_string(),
                }));
            return Ok(messages);
        }

        // 5a. task_done short-circuit — accepted per-mode:
        //     - Agent: always honored (one-shot generic completion signal).
        //     - Plan: ONLY honored after ExitPlanMode has actually fired, so
        //       a model trying to bail out of planning without writing the
        //       plan file + requesting approval cannot escape via task_done.
        //       If task_done is called early in Plan mode, its tool result
        //       is rewritten to a rejection note ("you must write the plan
        //       and call ExitPlanMode first") so the next LLM turn retries
        //       the proper path.
        //     - Dag: ONLY honored after a real dispatch tool fired, so an
        //       orchestrator trying to skip delegation can't escape via
        //       task_done. Same rejection pattern as Plan mode.
        let task_done_call_id: Option<String> = tool_uses.iter().find_map(|b| match b {
            ContentBlock::ToolUse { name, id, .. } if name == crate::tools::TASK_DONE_TOOL_NAME => {
                Some(id.clone())
            }
            _ => None,
        });
        if let Some(td_id) = task_done_call_id {
            // Honour a mid-turn switch into Plan mode here too (entered_plan_mode
            // is already set above if EnterPlanMode ran this turn).
            let td_mode = effective_agent_mode(config.agent_mode, entered_plan_mode);
            let accepted = match td_mode {
                AgentMode::Agent => true,
                AgentMode::Plan => exit_plan_mode_called,
                AgentMode::Dag => dispatch_executed,
            };
            if accepted {
                info!(
                    "[{}] task_done called by model — completing query",
                    config.agent_id
                );
                messages.push(assistant_msg);
                if !tool_results.is_empty() {
                    messages.push(create_user_message(tool_results));
                }
                return Ok(messages);
            }
            // Rejected: rewrite the task_done result so the model sees an
            // error and is forced to take the mode's canonical exit path.
            let reject_msg = match td_mode {
                AgentMode::Plan => {
                    "task_done was NOT accepted: in Plan mode the only valid exit \
                    is to write your plan file to the plans_dir and then call \
                    `ExitPlanMode(plan=\"...\")`. Do that now — task_done is reserved for \
                    abandoning the planning task entirely, not for finishing the plan."
                }
                AgentMode::Dag => {
                    "task_done was NOT accepted: in this mode you must first \
                    delegate work via `DispatchCreateParentAndRun(...)` (which blocks until \
                    sub-agents finish), then report results to the user. Calling task_done \
                    before any dispatch means no work was actually done."
                }
                AgentMode::Agent => unreachable!(),
            };
            for r in tool_results.iter_mut() {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = r
                {
                    if *tool_use_id == td_id {
                        *content = reject_msg.to_string();
                        *is_error = true;
                    }
                }
            }
            info!(
                "[{}] task_done rejected in {} mode (canonical signal not yet fired) — rewrote \
                 tool result, conversation continues",
                config.agent_id,
                td_mode.as_str()
            );
        }

        // 5b. Answer-and-stop: the model produced a real answer this turn AND
        //     every tool call it made was an intercepted duplicate (nothing
        //     fresh ran). There is no work left — deliver the answer instead of
        //     looping and re-generating it. Observed with weak models that emit
        //     the full answer plus a redundant tool call on every turn, so the
        //     "no tool calls → done" exit never triggers.
        if fresh.is_empty() && !dup_ids.is_empty() && !text_content.trim().is_empty() {
            info!(
                "[{}] answer present and all {} tool call(s) were duplicates — completing",
                config.agent_id,
                tool_uses.len()
            );
            messages.push(assistant_msg);
            if !tool_results.is_empty() {
                messages.push(create_user_message(tool_results));
            }
            return Ok(messages);
        }

        // 6. Stall detection: count consecutive turns that call the same
        //    tool(s) and emit no user-facing text. A weak model can spin on
        //    e.g. browser_search forever (observed: 6+ identical search turns,
        //    context ballooning 7k→72k tokens). Nudge once, then hard-stop.
        let sig = tool_names_sig(&tool_uses);
        if text_content.trim().is_empty() && Some(&sig) == last_sig.as_ref() {
            stall_streak += 1;
        } else {
            stall_streak = 0;
        }
        last_sig = Some(sig.clone());

        // 6b. Error-loop detection: consecutive turns whose freshly-run tool
        //     calls ALL errored on the same tool(s). Unlike the stall counter
        //     above this ignores narration text — a model that comments every
        //     turn while a tool keeps failing (e.g. `ssh_start_connect` →
        //     "Missing host, port, or user") never trips the text-based stall,
        //     so without this it would loop to the `max_turns` cap and then
        //     salvage the failures into a *fabricated* answer. Nudge to
        //     fix-or-report, then hard-stop with a reported error.
        let fresh_all_errored = fresh_results_all_errored(&tool_results, &fresh);
        if fresh_all_errored && Some(&sig) == last_error_sig.as_ref() {
            error_streak += 1;
        } else {
            error_streak = 0;
        }
        last_error_sig = if fresh_all_errored {
            Some(sig.clone())
        } else {
            None
        };

        let stall_hard_stop = stall_streak >= stall_limit * 2;
        let should_nudge = !nudged && stall_streak >= stall_limit;
        let error_hard_stop = error_streak >= stall_limit * 2;
        let error_should_nudge = !error_nudged && error_streak >= stall_limit;
        let hard_stop = stall_hard_stop || error_hard_stop;

        // 7. Recurse: append the assistant turn and the tool results. A stall
        //    hard-stop doesn't abort the session — it disables tools and forces
        //    one final answer turn so the gathered context isn't thrown away.
        messages.push(assistant_msg);
        if hard_stop {
            forcing_final = true;
        }
        if error_hard_stop {
            warn!(
                "[{}] tool-error loop not resolved ({} consecutive all-error turns on '{}') — reporting failure and forcing an honest final answer",
                config.agent_id, error_streak, sig
            );
            // Report the failure so schedules / the UI record a real error
            // instead of a silent (fabricated) success. The forced-final turn
            // below still runs so the user also gets a plain-language reply.
            config
                .event_bus
                .emit(EngineEvent::SessionError(SessionErrorData {
                    error_type: "tool_error_loop".to_string(),
                    error: SessionErrorDetail {
                        code: "TOOL_ERROR_LOOP".to_string(),
                        message: format!(
                            "Tool '{sig}' failed on {error_streak} consecutive turns; stopping so \
                             the failure is reported instead of looping or fabricating a result."
                        ),
                        // Structured so the failure ledger can mark exactly
                        // which tools were hard-stopped. Parsing it back out
                        // of `message` would break the first time that
                        // sentence is reworded.
                        details: Some(serde_json::json!({
                            "toolSig": sig,
                            "streak": error_streak,
                        })),
                    },
                }));
        } else if stall_hard_stop {
            warn!(
                "[{}] tool-call stall not resolved after nudge ({} consecutive turns) — forcing a final answer (tools disabled)",
                config.agent_id, stall_streak
            );
        }
        // Prefer the error-specific final nudge (which forbids fabricating
        // results) when we stopped because of a tool-error loop.
        let final_nudge = if error_hard_stop {
            TOOL_ERROR_FINAL_NUDGE
        } else {
            FINAL_ANSWER_NUDGE
        };
        if !tool_results.is_empty() {
            let mut blocks = tool_results;
            // Mid-turn user messages ride along with this round's tool results.
            inject_pending_inputs(&mut blocks, config);
            if hard_stop {
                blocks.push(ContentBlock::Text {
                    text: final_nudge.to_string(),
                });
            } else if error_should_nudge {
                error_nudged = true;
                info!(
                    "[{}] tool-error loop detected ({} consecutive all-error turns on '{}') — nudging to fix or report",
                    config.agent_id, error_streak, sig
                );
                blocks.push(ContentBlock::Text {
                    text: TOOL_ERROR_NUDGE.to_string(),
                });
            } else if should_nudge {
                nudged = true;
                info!(
                    "[{}] tool-call stall detected ({} consecutive turns on '{}') — nudging to finalize",
                    config.agent_id,
                    stall_streak,
                    last_sig.as_deref().unwrap_or("")
                );
                blocks.push(ContentBlock::Text {
                    text: STALL_NUDGE.to_string(),
                });
            }
            messages.push(create_user_message(blocks));
        } else if hard_stop {
            messages.push(create_user_message(vec![ContentBlock::Text {
                text: final_nudge.to_string(),
            }]));
        }

        debug!(
            "[{}] recurse — {} messages, {} tool results",
            config.agent_id,
            messages.len(),
            tool_uses.len()
        );
    }
}

// ============================================================================
// Content extraction helpers
// ============================================================================

pub(crate) fn extract_content(msg: &Message) -> (String, String, Vec<ContentBlock>) {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_uses = Vec::new();

    for block in &msg.message.content {
        match block {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::Thinking { thinking: t } => reasoning.push_str(t),
            ContentBlock::ToolUse { .. } => {
                tool_uses.push(block.clone());
            }
            _ => {}
        }
    }

    (text, reasoning, tool_uses)
}

// ============================================================================
// Token estimation (rough)
// ============================================================================

/// Sum of input + output tokens reported by the most recent assistant message
/// that carries real API usage. Returns `None` if no message reported usage
/// (e.g. local inference, or a fresh conversation). Mirrors TS `countTokens`.
fn real_token_usage(messages: &[Message]) -> Option<(u64, u64)> {
    messages.iter().rev().find_map(|m| match &m.usage {
        Some(u) if !u.is_empty() => Some((u.input(), u.output())),
        _ => None,
    })
}

/// Rough char-based token estimate over all text-bearing content. Fallback for
/// providers that don't report usage. ~4 chars ≈ 1 token.
fn estimate_tokens_by_chars(messages: &[Message]) -> u64 {
    let total_chars: usize = messages
        .iter()
        .flat_map(|m| m.message.content.iter())
        .map(|b| match b {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::Thinking { thinking } => thinking.len(),
            ContentBlock::ToolResult { content, .. } => content.len(),
            _ => 0,
        })
        .sum();
    (total_chars as f64 / 4.0).ceil() as u64
}

/// Compute conversation token usage for the UI gauge.
///
/// Mirrors TS `getTokens`: prefer real API usage from the latest assistant
/// message, and report `max_tokens` as the model's actual context window
/// (not a hardcoded constant). Falls back to a char estimate when no usage
/// was reported.
fn count_tokens(messages: &[Message], context_length: u32) -> UsageData {
    let max_tokens = if context_length > 0 {
        context_length as u64
    } else {
        DEFAULT_CONTEXT_LENGTH
    };

    if let Some((input, output)) = real_token_usage(messages) {
        return UsageData {
            use_tokens: input + output,
            max_tokens,
            prompt_tokens: input,
        };
    }

    let use_tokens = estimate_tokens_by_chars(messages);
    UsageData {
        use_tokens,
        max_tokens,
        prompt_tokens: use_tokens,
    }
}

/// Current input-token count for compaction decisions: real prompt tokens when
/// available, else the char-based estimate.
fn current_input_tokens(messages: &[Message]) -> u64 {
    match real_token_usage(messages) {
        Some((input, _)) => input,
        None => estimate_tokens_by_chars(messages),
    }
}

/// Whether the conversation should be proactively compacted before the next
/// LLM call. Mirrors TS `needsAutoCompact`.
fn needs_auto_compact(messages: &[Message], context_length: u32) -> bool {
    if messages.len() < 3 {
        return false;
    }
    let limit = if context_length > 0 {
        context_length as u64
    } else {
        DEFAULT_CONTEXT_LENGTH
    };
    let threshold = (limit as f64 * AUTO_COMPACT_THRESHOLD_RATIO) as u64;
    current_input_tokens(messages) >= threshold
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_use(name: &str, args: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse {
            id: "x".into(),
            name: name.into(),
            input: args,
        }
    }

    fn sample_status_input() -> AgentStatusInput {
        AgentStatusInput {
            todo_open: 1,
            todo_total: 3,
            call_index: 2,
            call_budget: 30,
            elapsed_secs: 5,
            pending_events: 0,
            original_goal: "fix the retry logic".into(),
        }
    }

    fn history_ending_in_tool_result() -> Vec<Message> {
        vec![
            create_user_message(vec![ContentBlock::Text { text: "please read app.py".into() }]),
            Message {
                msg_type: "assistant".to_string(),
                message: MessagePayload {
                    role: "assistant".to_string(),
                    content: vec![tool_use("Read", serde_json::json!({"file_path": "app.py"}))],
                },
                uuid: "asst-1".into(),
                usage: None,
            },
            create_user_message(vec![ContentBlock::ToolResult {
                tool_use_id: "x".into(),
                content: "file contents".into(),
                is_error: false,
            }]),
        ]
    }

    #[test]
    fn agent_status_off_returns_the_same_borrow_untouched() {
        // (d): with the switch off, the request is identical to before —
        // not just equal, the *same borrow*, so there is no clone at all.
        let messages = history_ending_in_tool_result();
        let out = outgoing_messages(&messages, false, &sample_status_input());
        assert!(matches!(out, Cow::Borrowed(_)), "off must never clone");
        assert_eq!(out.len(), messages.len());
    }

    #[test]
    fn agent_status_on_appends_exactly_one_trailing_block_to_the_last_user_message() {
        // (b): the outgoing request carries exactly one trailing status block.
        let messages = history_ending_in_tool_result();
        let before_last_len = messages.last().unwrap().message.content.len();
        let out = outgoing_messages(&messages, true, &sample_status_input());
        assert!(matches!(out, Cow::Owned(_)), "on must produce an owned clone");
        let last = out.last().unwrap();
        assert_eq!(last.message.role, "user", "tool results live in the user role");
        assert_eq!(last.message.content.len(), before_last_len + 1, "exactly one block added");
        match last.message.content.last().unwrap() {
            ContentBlock::Text { text } => {
                assert!(text.starts_with("<agent_status>"));
                assert!(text.contains("fix the retry logic"));
            }
            other => panic!("expected a trailing Text block, got {other:?}"),
        }
    }

    #[test]
    fn agent_status_never_mutates_the_persisted_history() {
        // (a): persisted history is byte-identical with the status on or off
        // — `outgoing_messages` takes `messages` by shared reference, so this
        // is a compile-time guarantee, not just a runtime one; this test
        // pins the *serialized* equality too, in case that ever changes.
        let messages = history_ending_in_tool_result();
        let before = serde_json::to_string(&messages).unwrap();
        let _ = outgoing_messages(&messages, true, &sample_status_input());
        let after = serde_json::to_string(&messages).unwrap();
        assert_eq!(before, after, "messages must be byte-identical after building either outgoing view");
    }

    #[test]
    fn a_static_prefix_hash_does_not_depend_on_agent_status() {
        // (c): the static-prefix hash is unchanged by the status.
        // `context_assembler::prefix_hash` only ever sees the system prompt +
        // tool schema — `outgoing_messages`'s signature has no way to reach
        // either, so the same static bytes hash identically regardless of
        // whether agent_status ran.
        let system_prompt = "SYSTEM_PROMPT + tool catalogue v1";
        let before = context_assembler::prefix_hash(system_prompt);
        let messages = history_ending_in_tool_result();
        let _ = outgoing_messages(&messages, true, &sample_status_input());
        let after = context_assembler::prefix_hash(system_prompt);
        assert_eq!(before, after);
    }

    #[test]
    fn goal_text_captures_the_original_real_user_message_not_a_tool_result_carrier() {
        let messages = history_ending_in_tool_result();
        assert_eq!(goal_text(&messages), "please read app.py");
    }

    #[test]
    fn tool_names_sig_is_stable_across_arg_changes() {
        // Same tool, different args (e.g. a tweaked search query) → same sig,
        // which is exactly what lets the stall detector catch a search loop.
        let a = tool_names_sig(&[tool_use("search", serde_json::json!({"q": "gold"}))]);
        let b = tool_names_sig(&[tool_use("search", serde_json::json!({"q": "gold price"}))]);
        assert_eq!(a, b);
        assert_eq!(a, "search");

        // Order-independent and deduped.
        let multi = tool_names_sig(&[
            tool_use("b", serde_json::json!({})),
            tool_use("a", serde_json::json!({})),
            tool_use("a", serde_json::json!({})),
        ]);
        assert_eq!(multi, "a,b");

        // Non-tool blocks ignored.
        assert_eq!(
            tool_names_sig(&[ContentBlock::Text { text: "hi".into() }]),
            ""
        );
    }

    #[test]
    fn tool_call_sig_distinguishes_args_not_just_name() {
        let a = tool_call_sig(&tool_use("search", serde_json::json!({"q": "gold"})));
        let a2 = tool_use("search", serde_json::json!({"q": "gold"}));
        let b = tool_call_sig(&tool_use("search", serde_json::json!({"q": "silver"})));
        // Identical name+args → identical signature (caught as duplicate).
        assert_eq!(a, tool_call_sig(&a2));
        // Different args → different signature (allowed through).
        assert_ne!(a, b);
        // Non-tool block → no signature.
        assert!(tool_call_sig(&ContentBlock::Text { text: "x".into() }).is_none());
    }

    #[test]
    fn fresh_all_errored_only_counts_freshly_run_calls() {
        let tu = |id: &str, name: &str| ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
        };
        let res = |id: &str, is_error: bool| ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: "x".into(),
            is_error,
        };

        // Single fresh call that errored → true.
        assert!(fresh_results_all_errored(
            &[res("a", true)],
            &[tu("a", "ssh_start_connect")]
        ));

        // Fresh call that succeeded → false.
        assert!(!fresh_results_all_errored(
            &[res("a", false)],
            &[tu("a", "ssh_start_connect")]
        ));

        // Mixed: one fresh error, one fresh success → not ALL errored → false.
        assert!(!fresh_results_all_errored(
            &[res("a", true), res("b", false)],
            &[tu("a", "t"), tu("b", "t")]
        ));

        // No fresh calls (all were duplicates) → false, even if a synthetic
        // duplicate note happens to be present.
        assert!(!fresh_results_all_errored(&[res("dup", false)], &[]));

        // A non-fresh (duplicate) success note must not mask a fresh error:
        // only the fresh id 'a' is inspected → true.
        assert!(fresh_results_all_errored(
            &[res("a", true), res("dup", false)],
            &[tu("a", "t")]
        ));
    }

    #[test]
    fn loop_guard_defaults_are_sane() {
        std::env::remove_var("SENCLAW_MAX_AGENT_TURNS");
        std::env::remove_var("SENCLAW_STALL_TOOL_TURNS");
        std::env::remove_var("SENCLAW_EMPTY_RETRIES");
        std::env::remove_var("SENCLAW_COMPLETION_NUDGES");
        assert_eq!(max_agent_turns(None), 30);
        assert_eq!(max_agent_turns(Some(60)), 60);
        assert_eq!(stall_tool_turns(), 4);
        assert_eq!(max_empty_retries(), 2);
        assert_eq!(max_completion_nudges(), 2);
    }

    #[test]
    fn dag_lead_tool_filter_keeps_orchestration_drops_worker_tools() {
        struct T {
            n: &'static str,
            ro: bool,
        }
        #[async_trait::async_trait]
        impl Tool for T {
            fn name(&self) -> &str {
                self.n
            }
            fn description(&self) -> &str {
                ""
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn is_read_only(&self) -> bool {
                self.ro
            }
            async fn call(
                &self,
                _i: serde_json::Value,
                _c: &ToolContext<'_>,
            ) -> Result<Vec<ToolOutput>> {
                Ok(vec![])
            }
            fn gen_tool_result_message(
                &self,
                _d: &serde_json::Value,
                _i: &serde_json::Value,
            ) -> ToolResultMessage {
                ToolResultMessage {
                    title: String::new(),
                    summary: String::new(),
                    content: serde_json::Value::Null,
                }
            }
            fn get_display_title(&self, _i: &serde_json::Value) -> String {
                String::new()
            }
        }
        let keep = |n: &'static str, ro: bool| is_dag_lead_tool(&T { n, ro });

        // Kept: read-only research builtins, dispatch + memory MCP, and the
        // non-read-only orchestration builtins the lead needs.
        assert!(keep("Read", true));
        assert!(keep("Grep", true));
        assert!(keep("ToolSearch", true));
        assert!(keep("DispatchCreateParentAndRun", false));
        assert!(keep("task_done", false));
        assert!(keep("TodoWrite", false));
        assert!(keep("WebFetch", false));
        assert!(keep("mcp__senclaw-dispatch__task", false));
        assert!(keep("mcp__senclaw-memory__memory_search", false));

        // Dropped: mutating builtins and the worker MCP surfaces (the 30-tool
        // browser server is the dominant bulk that trips endpoint tool limits).
        assert!(!keep("Bash", false));
        assert!(!keep("Write", false));
        assert!(!keep("Edit", false));
        assert!(!keep("mcp__senclaw-browser__browser_click", false));
        assert!(!keep("mcp__senclaw-wiki__wiki_write", false));
        assert!(!keep("mcp__senclaw-send__send_message", false));
    }

    #[test]
    fn agent_mode_never_nudges() {
        // Plain Agent mode (ordinary chat / scheduled tasks) keeps the legacy
        // "text = done" behavior — no nudge regardless of session state.
        assert!(completion_nudge_for(AgentMode::Agent, false, false, 1, 2).is_none());
        assert!(completion_nudge_for(AgentMode::Agent, true, true, 1, 2).is_none());
    }

    #[test]
    fn plan_mode_nudges_until_exit_plan_mode_called() {
        // Plan mode's natural completion is ExitPlanMode. Until that tool runs
        // this session, a text-only response is a premature stop.
        let pre = completion_nudge_for(AgentMode::Plan, false, false, 1, 2);
        assert!(pre.is_some(), "Plan mode should nudge before ExitPlanMode");
        let pre_text = pre.unwrap();
        assert!(pre_text.contains("ExitPlanMode"));
        assert!(pre_text.contains("(Nudge 1/2)"));

        // After ExitPlanMode runs, a follow-up text-only reply is a legitimate
        // wrap-up — no further nudge needed.
        let post = completion_nudge_for(AgentMode::Plan, true, false, 1, 2);
        assert!(
            post.is_none(),
            "Plan mode should NOT nudge after ExitPlanMode"
        );
    }

    #[test]
    fn mid_turn_enter_plan_mode_upgrades_effective_mode() {
        // A turn that STARTS in Agent mode but calls EnterPlanMode must behave as
        // Plan mode afterwards — otherwise a text-only reply silently ends the
        // turn and the approval dialog never appears.
        assert!(matches!(
            effective_agent_mode(AgentMode::Agent, false),
            AgentMode::Agent
        ));
        assert!(matches!(
            effective_agent_mode(AgentMode::Agent, true),
            AgentMode::Plan
        ));
        // Explicit Plan / Dag turns are unaffected by the flag.
        assert!(matches!(
            effective_agent_mode(AgentMode::Plan, false),
            AgentMode::Plan
        ));
        assert!(matches!(
            effective_agent_mode(AgentMode::Dag, true),
            AgentMode::Dag
        ));
        // End-to-end: Agent base + entered_plan_mode → the Plan nudge fires.
        let nudge = completion_nudge_for(
            effective_agent_mode(AgentMode::Agent, true),
            false,
            false,
            1,
            2,
        );
        assert!(nudge.unwrap().contains("ExitPlanMode"));
    }

    #[test]
    fn dag_mode_nudges_until_dispatch_executes() {
        // DAG orchestrator's natural completion path: dispatch (which blocks
        // until workers finish) → text report. Pre-dispatch text is premature.
        let pre = completion_nudge_for(AgentMode::Dag, false, false, 1, 2);
        assert!(pre.is_some(), "DAG mode should nudge before any dispatch");
        let pre_text = pre.unwrap();
        assert!(pre_text.contains("DispatchCreateParentAndRun"));
        assert!(pre_text.contains("(Nudge 1/2)"));

        // After dispatch returns, the orchestrator's job is to report results
        // — text-only is the natural exit. No nudge.
        let post = completion_nudge_for(AgentMode::Dag, false, true, 1, 2);
        assert!(
            post.is_none(),
            "DAG mode should NOT nudge after dispatch executed"
        );
    }

    #[test]
    fn plan_and_dag_track_independent_flags() {
        // Cross-flag: an ExitPlanMode call from a prior Plan session doesn't
        // satisfy DAG mode, and vice-versa.
        assert!(completion_nudge_for(AgentMode::Dag, true, false, 1, 2).is_some());
        assert!(completion_nudge_for(AgentMode::Plan, false, true, 1, 2).is_some());
    }

    #[test]
    fn task_done_acceptance_matrix() {
        // Mirror of the per-mode gate in the short-circuit branch. Documenting
        // here as a unit test so future tweaks have a single place to flip.
        fn accepted(mode: AgentMode, exit_plan: bool, dispatched: bool) -> bool {
            match mode {
                AgentMode::Agent => true,
                AgentMode::Plan => exit_plan,
                AgentMode::Dag => dispatched,
            }
        }

        // Agent: always accepted.
        assert!(accepted(AgentMode::Agent, false, false));
        assert!(accepted(AgentMode::Agent, true, true));

        // Plan: rejected until ExitPlanMode fired.
        assert!(!accepted(AgentMode::Plan, false, false));
        assert!(!accepted(AgentMode::Plan, false, true)); // dispatch is irrelevant here
        assert!(accepted(AgentMode::Plan, true, false));

        // Dag: rejected until dispatch fired.
        assert!(!accepted(AgentMode::Dag, false, false));
        assert!(!accepted(AgentMode::Dag, true, false)); // ExitPlanMode is irrelevant
        assert!(accepted(AgentMode::Dag, false, true));
    }

    #[test]
    fn executing_dispatch_tools_excludes_informational_ones() {
        // The list of tools that "count" as DAG progress must not include
        // read-only / setup-only dispatch helpers — those don't delegate work.
        assert!(EXECUTING_DISPATCH_TOOLS.contains(&"DispatchCreateParentAndRun"));
        assert!(EXECUTING_DISPATCH_TOOLS.contains(&"DispatchAllTasks"));
        assert!(EXECUTING_DISPATCH_TOOLS.contains(&"DispatchTask"));
        // These DON'T count — calling them alone doesn't kick off any work.
        assert!(!EXECUTING_DISPATCH_TOOLS.contains(&"DispatchListAgents"));
        assert!(!EXECUTING_DISPATCH_TOOLS.contains(&"DispatchCreateParent"));
    }

    #[test]
    fn extract_content_separates_text_and_tools() {
        let msg = Message {
            msg_type: "assistant".into(),
            message: MessagePayload {
                role: "assistant".into(),
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "Let me think...".into(),
                    },
                    ContentBlock::Text {
                        text: "I will help.".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "tu-1".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path": "/tmp/test"}),
                    },
                ],
            },
            uuid: "uuid-1".into(),
            usage: None,
        };

        let (text, reasoning, tools) = extract_content(&msg);
        assert_eq!(text, "I will help.");
        assert_eq!(reasoning, "Let me think...");
        assert_eq!(tools.len(), 1);
        if let ContentBlock::ToolUse { name, .. } = &tools[0] {
            assert_eq!(name, "read");
        } else {
            panic!("Expected ToolUse");
        }
    }

    #[test]
    fn extract_content_empty() {
        let msg = Message {
            msg_type: "assistant".into(),
            message: MessagePayload {
                role: "assistant".into(),
                content: vec![],
            },
            uuid: "uuid-1".into(),
            usage: None,
        };
        let (text, reasoning, tools) = extract_content(&msg);
        assert!(text.is_empty());
        assert!(reasoning.is_empty());
        assert!(tools.is_empty());
    }

    #[test]
    fn count_tokens_falls_back_to_char_estimate() {
        let msgs = vec![Message {
            msg_type: "user".into(),
            message: MessagePayload {
                role: "user".into(),
                content: vec![ContentBlock::Text {
                    text: "Hello, world!".into(),
                }],
            },
            uuid: "uuid-1".into(),
            usage: None,
        }];
        let usage = count_tokens(&msgs, 128_000);
        // 13 chars / 4 ≈ 4 tokens, and max_tokens reflects the model context.
        assert!(usage.use_tokens >= 3);
        assert_eq!(usage.max_tokens, 128_000);
    }

    #[test]
    fn count_tokens_prefers_real_usage() {
        let mut assistant = Message {
            msg_type: "assistant".into(),
            message: MessagePayload {
                role: "assistant".into(),
                content: vec![ContentBlock::Text {
                    text: "short".into(),
                }],
            },
            uuid: "a1".into(),
            usage: Some(RawUsage {
                input_tokens: Some(1000),
                output_tokens: Some(200),
                cache_read_input_tokens: Some(50),
                ..Default::default()
            }),
        };
        // Anthropic shape: input includes cache tokens.
        let usage = count_tokens(std::slice::from_ref(&assistant), 200_000);
        assert_eq!(usage.prompt_tokens, 1050);
        assert_eq!(usage.use_tokens, 1250);
        assert_eq!(usage.max_tokens, 200_000);

        // OpenAI shape on a later message wins (most recent).
        assistant.usage = Some(RawUsage {
            prompt_tokens: Some(900),
            completion_tokens: Some(100),
            ..Default::default()
        });
        let usage = count_tokens(std::slice::from_ref(&assistant), 200_000);
        assert_eq!(usage.prompt_tokens, 900);
        assert_eq!(usage.use_tokens, 1000);
    }

    #[test]
    fn needs_auto_compact_respects_threshold() {
        let make = |input: u64| Message {
            msg_type: "assistant".into(),
            message: MessagePayload {
                role: "assistant".into(),
                content: vec![ContentBlock::Text { text: "x".into() }],
            },
            uuid: "a".into(),
            usage: Some(RawUsage {
                input_tokens: Some(input),
                output_tokens: Some(1),
                ..Default::default()
            }),
        };
        // Need >= 3 messages and input >= 75% of context.
        let below = vec![make(10), make(20), make(70_000)];
        assert!(!needs_auto_compact(&below, 128_000));
        let above = vec![make(10), make(20), make(100_000)];
        assert!(needs_auto_compact(&above, 128_000));
        // Too few messages never compacts.
        assert!(!needs_auto_compact(&[make(200_000)], 128_000));
    }
}

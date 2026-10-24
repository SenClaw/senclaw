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

use super::hooks::{ErrorInput, HookEvent, HookInput, HookInputBase, HookManager, PreCompactInput, StopInput};
use super::{finish_check, stuck};
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
/// the model context window. Book Ch2 Policy 6 (adaptive window) uses ~80% so
/// early turns keep raw tool output intact and KV cache is not thrashed.
const AUTO_COMPACT_THRESHOLD_RATIO: f64 = 0.80;
/// Minimum message count before compaction is considered. Below this the
/// history is small enough that summarizing would cost an LLM call without
/// meaningful benefit (and risk losing recent detail).
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

/// How long one LLM turn may take before the loop gives up on it.
///
/// Local native inference (Candle CPU/Metal, MLX native) runs in-process and is much
/// slower than cloud APIs - MLX with TurboQuant KV blows past 180 s in prefill alone.
/// A Space-App provider (`app:<id>`) is a local engine behind the daemon's own loopback
/// proxy and is the same class. So is a server on the owner's own machine or network
/// (LM Studio on the LAN at 7-10 tok/s): a 100-line `Write` is over 180 s of steady
/// output, and the cut came as "LLM request timed out", which resets the session and
/// loses the work.
fn turn_timeout_for(adapt: &str, provider: &str, base_url: &str) -> Duration {
    if adapt.starts_with("local-") || provider.starts_with("app:") || query_llm::is_self_hosted_endpoint(base_url) {
        LLM_TURN_TIMEOUT_LOCAL
    } else {
        LLM_TURN_TIMEOUT
    }
}

/// Compact message history when context length is exceeded.
/// Keeps the first user message (original task framing) and the most recent messages,
/// dropping middle messages to reduce token count.
fn compact_messages(messages: &mut Vec<Message>) -> bool {
    if messages.len() <= COMPACT_KEEP_RECENT + 2 {
        return false;
    }
    let first_user_idx = messages.iter().position(|m| is_real_user_message(m));
    let mut keep_from = messages.len().saturating_sub(COMPACT_KEEP_RECENT);

    // Walk back so we never start mid tool_use / tool_result pair.
    while keep_from > 0 && is_tool_result_user_message(&messages[keep_from]) {
        keep_from -= 1;
    }

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
    sanitize_tool_pairs(&mut kept);
    *messages = kept;
    true
}

fn is_tool_result_user_message(m: &Message) -> bool {
    m.msg_type == "user"
        && matches!(
            m.message.content.first(),
            Some(ContentBlock::ToolResult { .. })
        )
}

fn is_real_user_message(m: &Message) -> bool {
    m.msg_type == "user" && !is_tool_result_user_message(m)
}

fn tool_use_ids_in(m: &Message) -> Vec<String> {
    m.message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// Drop orphan tool_result blocks whose matching tool_use is not in the list.
fn sanitize_tool_pairs(messages: &mut Vec<Message>) {
    let mut known_uses = std::collections::HashSet::new();
    for m in messages.iter() {
        for id in tool_use_ids_in(m) {
            known_uses.insert(id);
        }
    }
    messages.retain_mut(|m| {
        if !is_tool_result_user_message(m) {
            return true;
        }
        m.message.content.retain(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => known_uses.contains(tool_use_id),
            _ => true,
        });
        !m.message.content.is_empty()
    });
}

/// Prompt used to ask the main model for a lossless session snapshot.
/// Ported verbatim from TS `prompt/compact.ts` `COMPRESSION_PROMPT`.
const COMPRESSION_PROMPT: &str = "Create a lossless state snapshot of this session so any later instance can seamlessly resume work.

Cover the following (merge sections freely, but omit nothing):

A. **Intent evolution** — User requests in time order, how they changed, final shape. Include key user messages verbatim.
B. **Technical context** — Frameworks, toolchains, architecture, runtime environment.
C. **Artifacts & changes** — Files examined/modified/created. Embed full source for key changes. Keep file paths as recoverable references.
D. **Errors & fixes** — All anomalies, fix paths, and user corrections (failed approaches matter — do not drop them).
E. **Open items** — Closed vs in-progress vs remaining work, with blockers.
F. **Interruption point** — Exact files, functions, edit actions at the moment of interruption.
G. **Continuation path** (only if applicable) — Quote user's follow-up intent, task name, suggested handoff.
H. **Decisions & constraints** — Architecture choices, user preferences, hard constraints that later turns must respect.

## Rules
- Archive only from conversation content — no speculation or fabrication.
- Prioritize task-relevant facts over raw tool dumps; drop navigation chrome / ads / repeated noise.
- Keep semantic completeness (who/what/when/where) — never compress \"X left OpenAI in May 2024\" into \"X left\".
- When citing external facts, keep source URLs or file paths as references so originals can be re-fetched.
- Label gaps as \"not confirmed in context\".
- No tool calls — pure text reasoning and archival.
- Prefer full source over vague description for small critical snippets; otherwise summarize + path.
";

const COMPACT_NOTICE: &str = "[COMPRESSED] Context Compression Notice
The conversation was compressed to stay within the token budget and reduce context rot. Below is a structured summary; re-fetch originals via paths/URLs in the summary when detail is needed.";

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
    messages.iter().rposition(is_real_user_message)
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
/// Identical to `messages` — the same borrow, not even a clone — when there is
/// nothing to add: the request must then be byte-for-byte what it would have
/// been without these features. Otherwise a full clone with, appended to the
/// *last user-role* message (where tool results already live, per the internal
/// message format — see `docs/control-plane.md` §4):
///
/// 1. `turn_context` — what memory recalled for this turn, as one text block;
/// 2. `<agent_status>`, when `agent_status` is on.
///
/// `messages` itself is never touched, so the persisted history this
/// function's caller eventually returns, the trajectory recorder and
/// compaction's input are all unaffected. That is the point of the first block:
/// recall is re-derived from every new question, so keeping last turn's in the
/// history only re-sends it, and brings compaction forward.
fn outgoing_messages<'a>(
    messages: &'a [Message],
    agent_status_on: bool,
    input: &AgentStatusInput,
    turn_context: Option<&str>,
) -> Cow<'a, [Message]> {
    let context = turn_context.map(str::trim).filter(|c| !c.is_empty());
    if !agent_status_on && context.is_none() {
        return Cow::Borrowed(messages);
    }
    // No user-role message to attach to should not happen (the loop always
    // ends on one), but degrading to "unchanged" is safer than guessing a
    // different role to inject into.
    let Some(idx) = messages.iter().rposition(|m| m.message.role == "user") else {
        return Cow::Borrowed(messages);
    };
    let mut clone = messages.to_vec();
    if let Some(context) = context {
        clone[idx].message.content.push(ContentBlock::Text {
            text: context.to_string(),
        });
    }
    if agent_status_on {
        clone[idx].message.content.push(ContentBlock::Text {
            text: context_assembler::render(input),
        });
    }
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

/// **After-process / adaptive-window compaction.** Run after a turn (or on
/// manual force) with Claude-Code-style LLM summarization.
///
/// When `force` is false (normal after-process), compaction only runs once
/// input tokens reach [`AUTO_COMPACT_THRESHOLD_RATIO`] of the context window
/// *and* the history has at least [`AFTER_PROCESS_MIN_MESSAGES`] — matching
/// book Ch2 Policy 6 (batch compress near capacity, not every turn).
/// When `force` is true (user clicked Compact), the token threshold is skipped
/// but the minimum message floor still applies.
///
/// Earlier messages are summarized into a lossless snapshot while the most
/// recent turn is preserved verbatim; on summary failure it falls back to
/// keep-recent truncation (inherited from [`auto_compact`]). Emits the same
/// Compact/Usage events + Pre/PostCompact hooks as the in-loop path so the UI
/// reflects the compaction. Returns the (possibly unchanged) message list;
/// no-op for subagents and trivially short conversations.
pub async fn compact_now(
    messages: Vec<Message>,
    config: &QueryConfig,
    cancel: &CancellationToken,
) -> Vec<Message> {
    compact_now_inner(messages, config, cancel, false).await
}

/// Same as [`compact_now`] but skips the context-window threshold (manual Compact).
pub async fn compact_now_forced(
    messages: Vec<Message>,
    config: &QueryConfig,
    cancel: &CancellationToken,
) -> Vec<Message> {
    compact_now_inner(messages, config, cancel, true).await
}

async fn compact_now_inner(
    mut messages: Vec<Message>,
    config: &QueryConfig,
    cancel: &CancellationToken,
    force: bool,
) -> Vec<Message> {
    if config.is_subagent || messages.len() < AFTER_PROCESS_MIN_MESSAGES {
        return messages;
    }
    if !force && !needs_auto_compact(&messages, config.profile.context_length) {
        debug!(
            agent_id = %config.agent_id,
            msg_count = messages.len(),
            "after-process: skip compact — below adaptive-window threshold"
        );
        return messages;
    }
    info!(
        agent_id = %config.agent_id,
        msg_count = messages.len(),
        force,
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
                append_continuity_reminder(&mut truncated, &config.chat_jid);
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

    // Persist summary as handoff so resume / next turn can re-inject it even
    // if the in-memory summary message is later dropped.
    if let Some(ref summary_text) = summary {
        if !config.chat_jid.is_empty() {
            let ws = crate::control_plane::workspace::Workspace::for_chat(&config.chat_jid);
            if let Err(e) = ws.write_handoff(summary_text) {
                warn!(
                    chat_jid = %config.chat_jid,
                    error = %e,
                    "failed to write compaction handoff.md"
                );
            }
        }
    }

    // Usage after compaction reflects only the compacted history (notice +
    // summary); the kept current turn re-counts on the next real LLM turn.
    let usage_after = count_tokens(&compacted_history, ctx);
    let mut final_messages = compacted_history;
    final_messages.extend(keep);
    append_continuity_reminder(&mut final_messages, &config.chat_jid);

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

/// Inject date + workspace handoff/progress after compaction so the model
/// retains session continuity even when middle trajectory was dropped.
fn append_continuity_reminder(messages: &mut Vec<Message>, chat_jid: &str) {
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut parts: Vec<String> = vec![format!(
        "Context was compacted. Today's date is {date}."
    )];
    if !chat_jid.is_empty() {
        let ws = crate::control_plane::workspace::Workspace::for_chat(chat_jid);
        if let Some(handoff) = ws.read_handoff() {
            let body = crate::util::text::truncate_on_char_boundary(&handoff, 6_000);
            parts.push(format!("Session handoff (resume notes):\n{body}"));
        }
        if let Some(progress) = ws.read_progress() {
            let body = crate::util::text::truncate_on_char_boundary(&progress, 4_000);
            parts.push(format!("Session progress log:\n{body}"));
        }
    }
    let reminder = create_user_message(vec![ContentBlock::Text {
        text: format!(
            "<system-reminder>\n{}\n</system-reminder>",
            parts.join("\n\n")
        ),
    }]);
    // Place after the compact notice when present; otherwise after the first
    // real user message so the model sees continuity before recent turns.
    let insert_at = messages
        .iter()
        .position(|m| {
            m.message.content.iter().any(|b| matches!(
                b,
                ContentBlock::Text { text } if text.contains("[Context compacted")
                    || text.contains("[Context Compression Notice]")
            ))
        })
        .map(|i| i + 1)
        .or_else(|| {
            messages
                .iter()
                .position(is_real_user_message)
                .map(|i| i + 1)
        })
        .unwrap_or(0)
        .min(messages.len());
    messages.insert(insert_at, reminder);
}

/// Configuration passed to the query loop. All fields are owned so the config
/// can be moved into spawned tasks.
/// Resolver returning the current tool list. Called once per turn so newly
/// `ToolSearch`-discovered tools become available without recreating the
/// engine. Mirrors the same pattern used by `TaskTool::tools_resolver`.
pub type ToolsResolver = Arc<dyn Fn() -> Vec<Arc<dyn Tool>> + Send + Sync>;

pub struct QueryConfig {
    pub agent_id: String,
    /// Chat JID (engine instance id) — used to load workspace handoff/progress
    /// after compaction. Empty for subagents / unit tests.
    pub chat_jid: String,
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
    /// Recalled memory for this turn: appended to the last user-role message of
    /// each call's CLONE, never to `messages`. See [`outgoing_messages`].
    pub turn_context: Option<String>,
    /// Per-engine turn-budget override (see [`max_agent_turns`]). `None` = default.
    pub max_turns_override: Option<usize>,
    /// Current agent mode (Agent / Plan / Dag). Drives `task_done` enforcement:
    /// Plan and Dag modes nudge the model toward `task_done` when it stops with
    /// text only. Agent mode keeps legacy "text = done" behavior so ordinary
    /// chat is unaffected.
    pub agent_mode: AgentMode,
    /// A coding session (a code session, a git repository, a project chat).
    /// Turns on what a coding run needs and a chat does not: a check before
    /// finishing ([`verify_nudge`], a failed check, the optional judge), the
    /// observation-aware stuck detector instead of the name-based stall guard.
    /// Never set for subagents or compaction.
    pub coding_session: bool,
    /// Stop the run once it has processed this many tokens (input + output, every
    /// call of this user input). `None` = no cap.
    pub max_run_tokens: Option<u64>,
    /// Before a coding session ends, ask the model whether the work is finished
    /// ([`finish_check`]). One extra call per finish, off by default.
    pub finish_judge: bool,
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

/// A compaction that just ran is not repeated for this many turns: the next
/// one has to be earned by the context growing back, not by a stale gauge.
const COMPACT_REARM_TURNS: usize = 8;

/// A stuck nudge is forgiven after this many turns; a second one sooner stops the run.
const STUCK_FORGIVE_TURNS: usize = 25;

/// Whether running this call may change what an identical later call returns:
/// an edit, a write, a subagent, or a command that is not read-only.
fn may_change_workspace(block: &ContentBlock) -> bool {
    let ContentBlock::ToolUse { name, input, .. } = block else { return false };
    match name.as_str() {
        "Edit" | "Write" | "NotebookEdit" | "Task" => true,
        "Bash" => !input
            .get("command")
            .and_then(|v| v.as_str())
            .is_some_and(crate::util::shell_safety::is_readonly_safe_command),
        _ => false,
    }
}

/// `Bash: cargo test`, `Edit: /p/src/a.rs`: a call as a person would name it.
fn call_label(name: &str, input: &serde_json::Value) -> String {
    let key = ["command", "file_path", "notebook_path", "path", "pattern", "query", "url", "symbol", "name"]
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()));
    match key {
        Some(v) => {
            let v: String = v.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect();
            format!("{name}: {v}")
        }
        None => name.to_string(),
    }
}

/// The text of the user's request: the last text block of the last real user message.
fn last_user_prompt(messages: &[Message]) -> String {
    let Some(i) = last_real_user_index(messages) else { return String::new() };
    messages[i]
        .message
        .content
        .iter()
        .rev()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Ask the model whether the request is finished. `None` on any failure.
async fn run_finish_judge(
    config: &QueryConfig,
    request: &str,
    evidence: &[String],
    answer: &str,
    cancel: &CancellationToken,
) -> Option<finish_check::Verdict> {
    let msgs = vec![create_user_message(vec![ContentBlock::Text {
        text: finish_check::judge_prompt(request, evidence, answer),
    }])];
    let started = Instant::now();
    let reply = query_llm::query_llm(
        &config.http_client,
        &msgs,
        finish_check::JUDGE_SYSTEM,
        &[],
        cancel,
        &config.profile,
        false,
        false,
        None,
    )
    .await
    .ok()?;
    if let Some(u) = reply.usage.as_ref().filter(|u| !u.is_empty()) {
        config.event_bus.emit(EngineEvent::LlmUsage(LlmUsageData {
            source: "finish_judge".to_string(),
            agent_id: config.agent_id.clone(),
            session_id: config.session_id.clone(),
            profile: config.profile.name.clone(),
            provider: config.profile.provider.clone(),
            model: config.profile.model_name.clone(),
            usage: u.clone(),
            latency_ms: started.elapsed().as_millis() as u64,
            ok: true,
        }));
    }
    let (text, _reasoning, _tools) = extract_content(&reply);
    finish_check::parse_verdict(&text)
}

/// Times a coding request is sent back because its reply stopped without acting: it ended on a
/// colon, or - before any tool call - on a question or a block of code.
const MAX_ANNOUNCE_NUDGES: u8 = 2;

/// A reply that stops on a colon announces what comes next - "Đây là nội dung cho
/// file `inventory/service.py`:" - and then nothing does: the model meant to call a
/// tool, ended the turn first, and the loop read a turn with no tool call as the
/// final answer. gemma-4-e4b did it on two requests in a row (a 7-file project
/// stopped after 2 files); the same model called `Write` three times once told "you
/// only described it". Language-free on purpose: no phrase list, just the colon (with
/// markdown emphasis and a full-width colon allowed).
fn announces_without_acting(text: &str) -> bool {
    let t = text.trim_end_matches(|c: char| c.is_whitespace() || c == '*' || c == '_');
    t.ends_with(':') || t.ends_with('：')
}

fn announce_nudge() -> String {
    format!(
        "{}Your reply ends with a colon: it announces something and then makes no tool call, so nothing happened. \
Make the call now (Write, Edit or Bash) instead of describing it, and carry on until the whole request is done. \
If everything is already done, answer again with the final result and no trailing colon.",
        stuck::AUTOMATIC
    )
}

/// A run that has edited and run nothing yet and answers with a question or a block of code has
/// done nothing: it asked the person to approve a plan the request already asked for
/// ("Bạn đồng ý với kế hoạch này chứ?"), or wrote the calls it meant to make as text
/// (```bash read_file "models.py"```). gemma-4-e4b did one of the two on both of its first
/// two samples of a three-file edit, before any tool call, in 17 and 28 seconds. After the
/// run has acted a question or a block of code is an ordinary end of an answer, so this is
/// asked only of a run that has not yet edited a file or run a command (a read-only look
/// around - one `repo_map` call - does not count as acting: a third sample did exactly that
/// and stopped).
fn asks_or_pastes_instead_of_acting(text: &str) -> bool {
    let t = text.trim_end_matches(|c: char| c.is_whitespace() || c == '*' || c == '_');
    t.ends_with('?') || t.ends_with('？') || t.ends_with("```")
}

/// The files a request names: tokens that end in a source, document or config extension
/// (`inventory/models.py`, `README.md`). Words, versions and domain names are not files
/// (`e.g.`, `v1.2`, `example.com`). At most six, in the order they appear.
///
/// Two more samples of that three-file edit ended without a tool call and with neither
/// a question mark nor a code block ('Xin vui lòng xác nhận kế hoạch này trước khi tôi
/// tiến hành.' and a '?' in the middle of the last paragraph), so how the answer ends
/// cannot be the only test. What every one of them had in common is the request: it named
/// files, and the run opened none of them.
fn files_named_in(request: &str) -> Vec<String> {
    const EXTENSIONS: &[&str] = &["md", "json", "toml", "yaml", "yml", "txt", "html", "css", "sql", "dart", "swift", "kt", "kts", "rb", "php", "vue", "svelte", "scala", "sh", "lua", "zig"];
    let mut found: Vec<String> = Vec::new();
    for raw in request.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\'' | '`' | ':')) {
        let tok = raw.trim_end_matches(|c: char| matches!(c, '.' | '?' | '!'));
        let Some((stem, ext)) = tok.rsplit_once('.') else { continue };
        let name = stem.rsplit('/').next().unwrap_or(stem);
        let is_ext = !ext.is_empty() && ext.len() <= 6 && ext.chars().all(|c| c.is_ascii_alphanumeric());
        let known = EXTENSIONS.contains(&ext) || crate::repo_map::lang::spec_for_path(tok).is_some();
        if is_ext && known && name.chars().any(|c| c.is_alphabetic()) && !tok.contains("://") && !found.iter().any(|f| f == tok) {
            found.push(tok.to_string());
            if found.len() == 6 {
                break;
            }
        }
    }
    found
}

fn no_approval_nudge(unopened: &[String]) -> String {
    let files = if unopened.is_empty() { String::new() } else { format!("The request names {}, and you have not opened {}. ", unopened.join(", "), if unopened.len() == 1 { "it" } else { "them" }) };
    format!(
        "{}You have edited and run nothing yet, and your answer stops short of the work, so nothing was done. {files}\
The person has already asked for this work: do not ask for approval or confirmation, and do not write commands out as text. \
Call the tools now (Read the files first if you need to see them). Only if you cannot go on without information the request \
does not contain, say in one sentence what is missing.",
        stuck::AUTOMATIC
    )
}

/// Times a coding session is sent back after editing source files without running
/// anything that checks them. A small model's commonest failure is "done" on code
/// it never ran; OpenHands' goal judge and SWE-bench prompt both treat "claimed but
/// unverified" as not done. Two rounds, never a loop: the second names the exact
/// call to make, and an answer that still skips it ends the run with
/// [`unverified_warning`] attached.
const MAX_VERIFY_NUDGES: u8 = 2;

/// The note that sends the agent back to run a check. `command` is a call that
/// works in this folder (see [`suggested_check`]); with none, the generic form.
///
/// Worded for a model that reads every user-role message as the user speaking: a
/// real run's final reply was "the user is reminding me that…" until the note said
/// what it is and what the final answer should contain. And a model that ran a
/// check in an earlier request argues that run still counts, so the note says the
/// edits came after it.
fn verify_nudge(round: u8, command: Option<&str>) -> String {
    let auto = stuck::AUTOMATIC;
    match (round, command) {
        (1, Some(cmd)) => format!(
            "{auto}You edited source files after your last check, so no result you have seen covers the files as they are now. \
Run `{cmd}` with the Bash tool and fix what it reports. If it cannot be run here, say so in one sentence and why. \
Then write your final answer: what you changed (path:line) and what you ran, with its result."
        ),
        (1, None) => format!(
            "{auto}You edited source files but have not run anything that checks them. Run the narrowest build, test or \
lint command that covers your change and fix what it reports. If nothing can be run here, say so in one sentence and why. \
Then write your final answer: what you changed (path:line) and what you ran, with its result."
        ),
        (_, Some(cmd)) => format!(
            "{auto}You answered without running a check, so what you claim is not confirmed. Do not write another answer yet: \
call the Bash tool now with `{cmd}`, then report its real output."
        ),
        (_, None) => format!(
            "{auto}You answered without running a check, so what you claim is not confirmed. Do not write another answer yet: \
call the Bash tool now with the narrowest build, test or lint command that covers your change, then report its real output. \
If nothing can be run here, say that in one sentence."
        ),
    }
}

/// What the person reads after an answer that edited source and never checked it,
/// in the language of the request.
fn unverified_warning(command: Option<&str>, vietnamese: bool) -> String {
    match (vietnamese, command) {
        (true, Some(c)) => format!("⚠️ Chưa kiểm chứng: đã sửa mã nguồn nhưng không có lệnh build/test/lint nào chạy sau đó, nên kết quả trên chưa được xác nhận. Hãy chạy `{c}` trước khi dùng."),
        (true, None) => "⚠️ Chưa kiểm chứng: đã sửa mã nguồn nhưng không có lệnh build/test/lint nào chạy sau đó, nên kết quả trên chưa được xác nhận. Hãy tự chạy test hoặc build trước khi dùng.".to_string(),
        (false, Some(c)) => format!("⚠️ Not verified: source files were edited and no build/test/lint command ran afterwards, so the result above is unconfirmed. Run `{c}` before relying on it."),
        (false, None) => "⚠️ Not verified: source files were edited and no build/test/lint command ran afterwards, so the result above is unconfirmed. Run the tests or the build before relying on it.".to_string(),
    }
}

/// Times a coding request is sent back because an `Edit` / `Write` file it tried was never changed.
const MAX_UNAPPLIED_NUDGES: u8 = 2;

/// The edits a request tried, file by file. A model whose `old_string` keeps missing ('Error:
/// old_string not found', 9 to 13 `Edit` calls of one run on gemma-4-e4b) answers as if the
/// change were made. When all of its edits are refused there is nothing to check, so the verify
/// reminder, which needs a successful edit, never speaks; when only some are, the files that were
/// refused every time are exactly the ones the answer is wrong about (a run changed `service.py`
/// and left `models.py`, `storage.py`, `cli.py` and the tests as they were).
#[derive(Default)]
struct EditTally {
    /// (file as the call gave it, whether the call changed it)
    files: Vec<(String, bool)>,
    /// The start of the newest refusal, for the reminder: it says what to copy from.
    last_refusal: String,
}

/// One file, whether it was given as `models.py`, `./models.py` or `/abs/path/models.py`.
fn same_file(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_start_matches("./"), b.trim_start_matches("./"));
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

impl EditTally {
    fn record(&mut self, path: &str, failed: bool, result: &str) {
        self.files.push((path.to_string(), !failed));
        if failed {
            self.last_refusal = result.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect();
        }
    }

    /// Files an edit was tried on and not one of those edits changed, in the order they were tried.
    fn unchanged(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (path, _) in &self.files {
            let applied = self.files.iter().any(|(p, ok)| *ok && same_file(p, path));
            if !applied && !out.iter().any(|o| same_file(o, path)) {
                out.push(path.clone());
            }
        }
        out
    }
}

/// `models.py` rather than `/long/path/to/models.py`, for a note a person reads.
fn short_names(paths: &[String]) -> String {
    paths.iter().map(|p| p.rsplit('/').next().unwrap_or(p).to_string()).collect::<Vec<_>>().join(", ")
}

fn unapplied_edits_nudge(unchanged: &[String], last_refusal: &str) -> String {
    let why = if last_refusal.is_empty() { String::new() } else { format!(" The last refusal said: \"{last_refusal}\".") };
    format!(
        "{}Every Edit or Write you tried on {} was refused, so {} not changed - whatever you say you changed there is not there.{why} \
Read the file again and copy `old_string` exactly from what Read returns (or replace the whole file with Write), then check the result. \
If you cannot, say so plainly in your answer instead of describing the change as made.",
        stuck::AUTOMATIC,
        short_names(unchanged),
        if unchanged.len() == 1 { "that file is" } else { "those files are" }
    )
}

/// What the person reads after an answer that follows edits that were only ever refused.
fn unapplied_edits_warning(unchanged: &[String], vietnamese: bool) -> String {
    let files = short_names(unchanged);
    if vietnamese {
        format!("⚠️ Chưa áp dụng được thay đổi nào cho: {files}. Mọi lệnh Edit/Write vào các file này đều bị từ chối, nên chúng vẫn như cũ và câu trả lời trên không mô tả đúng code hiện tại.")
    } else {
        format!("⚠️ No change was applied to: {files}. Every Edit/Write on these files was refused, so they are unchanged and the answer above does not describe them.")
    }
}

/// The last build/test/lint the model itself ran in this conversation (earlier
/// requests included): a call known to work in this folder.
fn last_check_command(messages: &[Message]) -> Option<String> {
    messages.iter().rev().flat_map(|m| m.message.content.iter().rev()).find_map(|b| match b {
        ContentBlock::ToolUse { name, input, .. } if name == "Bash" => {
            input.get("command").and_then(|v| v.as_str()).filter(|c| is_verification_command(c)).map(|c| c.trim().to_string())
        }
        _ => None,
    })
}

/// The command to name when sending the agent back to check its edits: the last
/// check it ran itself, else the test (or first) command the folder's files
/// support, preferring the stack that holds the file it edited last.
fn suggested_check(working_dir: &str, messages: &[Message], edited: Option<&str>) -> Option<String> {
    if let Some(c) = last_check_command(messages) {
        return Some(c);
    }
    let stacks = crate::repo_map::commands::detect(std::path::Path::new(working_dir));
    let holds_edit = |s: &&crate::repo_map::commands::Stack| !s.dir.is_empty() && edited.map_or(false, |p| p.contains(&format!("/{}/", s.dir)));
    let stack = stacks.iter().find(holds_edit).or_else(|| stacks.first())?;
    stack.commands.iter().find(|(p, _)| *p == "test").or_else(|| stack.commands.first()).map(|(_, c)| c.clone())
}

/// Source files worth a build/test after an edit: not notes, not config.
fn is_source_edit(tool: &str, input: &serde_json::Value) -> bool {
    if !matches!(tool, "Edit" | "Write" | "NotebookEdit") {
        return false;
    }
    let path = input
        .get("file_path")
        .or_else(|| input.get("notebook_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let ext = path.rsplit('.').next().unwrap_or("");
    crate::repo_map::lang::spec_for_path(path).is_some()
        || matches!(ext, "dart" | "swift" | "kt" | "kts" | "rb" | "php" | "vue" | "svelte" | "scala" | "sh" | "lua" | "zig")
}

/// A command that builds, tests, lints or type-checks. Loose on purpose: a
/// model that ran *any* check has done what the nudge asks.
fn is_verification_command(cmd: &str) -> bool {
    const WORDS: &[&str] = &[
        "cargo test", "cargo check", "cargo build", "cargo clippy", "cargo nextest", "cargo fmt --check", "npm test", "npm run test",
        "npm run build", "npm run lint", "npm run typecheck", "npm run check", "pnpm test", "pnpm run", "pnpm build", "yarn test", "yarn build",
        "yarn lint", "bun test", "bun run", "npx tsc", "tsc", "eslint", "vitest", "jest", "pytest", "python -m pytest", "python3 -m pytest", "unittest",
        "ruff", "mypy", "pyright", "go test", "go build", "go vet", "make test", "make check", "make build", "make lint", "flutter test",
        "flutter analyze", "dart test", "dart analyze", "swift test", "swift build", "mvn test", "mvn package", "gradle test", "gradle build",
        "gradlew test", "gradlew build", "dotnet test", "dotnet build", "bundle exec rspec", "rspec", "rake test", "phpunit", "composer test",
    ];
    let c = cmd.to_ascii_lowercase();
    WORDS.iter().any(|w| c.contains(w)) || runs_a_program(&c)
}

/// A command that runs a script or a program of the project - `python3 check.py`,
/// `node test.js`, `./scripts/verify.sh`, `make`. The edit it follows was run, which
/// is all the verify nudge asks; whether the run proved anything is for the model
/// (and the failed-check nudge) to say.
fn runs_a_program(cmd: &str) -> bool {
    const RUNNERS: &[&str] = &["python", "python3", "node", "deno", "bun", "ruby", "php", "perl", "java", "bash", "sh", "zsh", "make", "just"];
    cmd.split(|c| matches!(c, '&' | ';' | '|')).any(|seg| {
        let mut words = seg.split_whitespace().skip_while(|w| w.contains('=') && !w.starts_with('-'));
        match words.next() {
            Some(first) if first.starts_with("./") => true,
            Some(first) if RUNNERS.contains(&first.rsplit('/').next().unwrap_or(first)) => {
                matches!(first, "make" | "just") || words.next().is_some()
            }
            _ => false,
        }
    })
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

/// Most of the person's request carried across a "clear context and start fresh" restart.
const RESTART_REQUEST_MAX_CHARS: usize = 8000;

/// The one message a run starts from after the person approved a plan with "clear context and
/// start fresh". The history is gone, so this message has to carry the request itself: a run that
/// starts from the plan alone (or, when the model left `plan` empty, from nothing) has no task,
/// and a model with no task reaches for whatever its skills advertise. The request goes in as the
/// person wrote it, so the reply follows their language - the wrapper is English like the other
/// automatic messages and names no language of its own.
fn restart_prompt(request: &str, plan: &str) -> String {
    let request = request.trim();
    let plan = plan.trim();
    let mut out = String::from(
        "(Automatic message - not typed by the person. The conversation so far was cleared because they approved the plan with \"clear context and start fresh\"; everything you need is below.)\n\n",
    );
    if !request.is_empty() {
        let request: String = request.chars().take(RESTART_REQUEST_MAX_CHARS).collect();
        out.push_str(&format!("The person's request:\n{request}\n\n"));
    }
    if plan.is_empty() {
        out.push_str("The approved plan: none was written down. The request above is the task - work out the steps yourself.\n\n");
    } else {
        out.push_str(&format!("The approved plan:\n{plan}\n\n"));
    }
    out.push_str("Carry it out now with your tools, and answer in the language of the request.");
    out
}

/// Times an answer is sent back because a skill that works through tools was loaded and no tool
/// was called afterwards.
const MAX_SKILL_NUDGES: u8 = 2;

/// Calls that load, plan or ask but fetch, change and run nothing: a skill is still unused while
/// these are all that has been called since it loaded.
const NON_ACTING_TOOLS: &[&str] = &[
    "Skill",
    "ToolSearch",
    "TodoWrite",
    "EnterPlanMode",
    "ExitPlanMode",
    "AskUser",
    "AskUserQuestion",
    "FormUI",
    "PersonaUpdate",
    "task_done",
];

/// What `skill_unused` becomes after one fresh call and its result: loading a skill that names
/// tools arms it, any call that fetches or runs something disarms it, and the rest (loading
/// another skill without tools, `ToolSearch`, planning) leave it as it was.
fn track_skill_use(current: Option<String>, tool: &str, input: &serde_json::Value, result: &str, failed: bool) -> Option<String> {
    if tool == "Skill" {
        if !failed && !crate::tools::tool_search::mcp_mentions(result).is_empty() {
            return Some(input.get("skill").and_then(|v| v.as_str()).unwrap_or("").to_string());
        }
        return current;
    }
    if NON_ACTING_TOOLS.contains(&tool) {
        current
    } else {
        None
    }
}

/// A skill that tells the model to call tools, loaded, and then an answer with no tool call in
/// between: the answer's figures, dates and links come from the model's memory. Loading a skill
/// performs nothing, and a small model answers straight after it anyway - one asked for today's
/// gold price loaded `web-research` and answered with invented prices and sources.
fn skill_unused_nudge(skill: &str) -> String {
    format!(
        "{}You loaded the skill `{skill}`, which does its work through tools, and have called none since: nothing in your answer comes from a live source, so every figure, date and link in it is from memory. \
Call the tools the skill names now (search, then read the pages) and answer from what they return. If they cannot be used here, say that in one sentence instead of giving figures.",
        stuck::AUTOMATIC
    )
}

/// What the person reads after such an answer that was sent back and still skipped the tools.
fn skill_unused_warning(skill: &str, vietnamese: bool) -> String {
    if vietnamese {
        format!("⚠️ Chưa tra cứu thật: skill `{skill}` đã được nạp nhưng không có công cụ nào được gọi sau đó, nên số liệu, ngày tháng và đường dẫn ở trên không lấy từ nguồn trực tiếp. Đừng dựa vào chúng.")
    } else {
        format!("⚠️ Not looked up: the skill `{skill}` was loaded but no tool was called afterwards, so the figures, dates and links above do not come from a live source. Do not rely on them.")
    }
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
const DUPLICATE_CALL_NOTE: &str = "Duplicate call: you already made this exact call and nothing has been edited or run \
that could change its result since, so it would return the same thing - the result is above. Use it. If you need \
something different, change the arguments, or change the files first.";

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
    let mut last_compact_turn: Option<usize> = None;
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
    // with the epoch they ran in, so an identical re-issue can be short-circuited
    // *when nothing has changed since*. An epoch ends whenever a call that may
    // change the workspace ran (an edit, a command that is not read-only): the
    // test–fix–retest cycle repeats an identical `cargo test` and must be run.
    let mut seen_tool_sigs: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut epoch: u64 = 0;
    // Stuck detection by what calls returned (coding sessions): nudge once, then stop.
    let mut detector = stuck::Detector::default();
    let mut stuck_nudge_turn: Option<usize> = None;
    // Tokens processed by this user input so far, for the run budget.
    let mut run_tokens: u64 = 0;
    // Finish checks (coding sessions): the last build/test/lint that failed and
    // nothing has passed since, what the run did (for the judge), and how many
    // times each check has sent the agent back.
    let mut last_check_failed: Option<String> = None;
    let mut failed_check_nudged = false;
    let mut judge_rounds: u8 = 0;
    let mut stop_gate_rounds: u8 = 0;
    let mut evidence = finish_check::Evidence::default();
    let original_prompt = last_user_prompt(&messages);
    // Resilience: retry transient EMPTY_COMPLETION before failing the session.
    let max_empty_retries_cap = max_empty_retries();
    let mut empty_retries: u8 = 0;
    // Mode-specific completion tracking. Plain Agent mode keeps the legacy
    // "text = done" behavior; Plan tracks `ExitPlanMode`, Dag tracks dispatch.
    // See `completion_nudge_for` for the per-mode semantics.
    let max_completion_nudges_cap = max_completion_nudges();
    let mut completion_nudges: u8 = 0;
    // Verify-before-finish (coding sessions): set by a source-file edit,
    // cleared by a build/test/lint command that runs after it.
    let mut unverified_edit = false;
    let mut verify_nudges: u8 = 0;
    let mut announce_nudges: u8 = 0;
    // The skill loaded this request that works through tools, until a tool is called after it.
    let mut skill_unused: Option<String> = None;
    let mut skill_nudges: u8 = 0;
    // Set once the run has edited a file or run a command (whether or not it worked), and what
    // its calls were given (a call label is cut at 80 characters, a path often is not shorter).
    let mut acted = false;
    let mut touched: Vec<String> = Vec::new();
    let mut edits = EditTally::default();
    let mut unapplied_nudges: u8 = 0;
    let mut last_edit_path: Option<String> = None;
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

        // Token budget: a second backstop beside the turn cap, for the case that
        // matters more on a hosted model - a long run that keeps going while the
        // bill grows. Like the turn cap it forces one final answer instead of
        // throwing the work away.
        if let Some(cap) = config.max_run_tokens {
            if run_tokens >= cap && !forcing_final {
                // No SessionError here: the pool treats one as the end of the request, and the
                // point is the final answer that follows. The answer says why it stopped.
                warn!("[{}] run token budget reached ({run_tokens} >= {cap}) - forcing a final answer (tools disabled)", config.agent_id);
                forcing_final = true;
                messages.push(create_user_message(vec![ContentBlock::Text { text: FINAL_ANSWER_NUDGE.to_string() }]));
            }
        }

        // 0. Proactive compaction: if the context is approaching the model
        //    limit, summarize the history *before* the next call so we don't
        //    hit a hard context_length error. Runs at most once per user input
        //    (the reactive path below remains as a safety net). Subagents skip.
        if !config.is_subagent
            && last_compact_turn.map_or(true, |t| turn.saturating_sub(t) >= COMPACT_REARM_TURNS)
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
            last_compact_turn = Some(turn);

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
        let outgoing = outgoing_messages(
            &messages,
            config.agent_status,
            &status_input,
            config.turn_context.as_deref(),
        );
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
        let turn_timeout = turn_timeout_for(
            config.profile.adapt.as_deref().unwrap_or(""),
            &config.profile.provider,
            &config.profile.base_url,
        );
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
                    run_tokens += u.input() + u.output();
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
                    if did_compact {
                        append_continuity_reminder(&mut messages, &config.chat_jid);
                    }
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

        // The answer that ends a coding run with edited source nobody checked, after the
        // agent was sent back to check it: the reply carries the warning. It goes on the
        // reply the person reads, not on the message kept for the model, which would copy it.
        let reported = if tool_uses.is_empty() && config.coding_session && !config.is_subagent && !forcing_final && unverified_edit && verify_nudges >= MAX_VERIFY_NUDGES {
            let command = suggested_check(&config.working_dir, &messages, last_edit_path.as_deref());
            let vietnamese = crate::docs::has_vietnamese_diacritics(&original_prompt) || crate::docs::has_vietnamese_diacritics(&text_content);
            warn!("[{}] finishing with edited source and no check run - warning attached to the answer", config.agent_id);
            format!("{}\n\n{}", text_content.trim_end(), unverified_warning(command.as_deref(), vietnamese))
        } else if tool_uses.is_empty() && config.coding_session && !config.is_subagent && !forcing_final && unapplied_nudges >= MAX_UNAPPLIED_NUDGES && !edits.unchanged().is_empty() {
            let vietnamese = crate::docs::has_vietnamese_diacritics(&original_prompt) || crate::docs::has_vietnamese_diacritics(&text_content);
            let unchanged = edits.unchanged();
            warn!("[{}] finishing with edits refused on {} - warning attached to the answer", config.agent_id, short_names(&unchanged));
            format!("{}\n\n{}", text_content.trim_end(), unapplied_edits_warning(&unchanged, vietnamese))
        } else if let Some(skill) = skill_unused.as_ref().filter(|_| tool_uses.is_empty() && !config.is_subagent && !forcing_final && skill_nudges >= MAX_SKILL_NUDGES) {
            let vietnamese = crate::docs::has_vietnamese_diacritics(&original_prompt) || crate::docs::has_vietnamese_diacritics(&text_content);
            warn!("[{}] finishing after loading skill {skill} with no tool called - warning attached to the answer", config.agent_id);
            format!("{}\n\n{}", text_content.trim_end(), skill_unused_warning(skill, vietnamese))
        } else {
            text_content.clone()
        };

        config
            .event_bus
            .emit(EngineEvent::MessageComplete(MessageCompleteData {
                agent_id: config.agent_id.to_string(),
                reasoning,
                content: reported,
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
            if !config.is_subagent && !forcing_final {
                // A skill that works through tools was loaded and nothing was called after it.
                if skill_nudges < MAX_SKILL_NUDGES {
                    if let Some(skill) = skill_unused.clone() {
                        skill_nudges += 1;
                        info!("[{}] answered right after loading skill {skill}, no tool called - sending the agent back ({skill_nudges}/{MAX_SKILL_NUDGES})", config.agent_id);
                        messages.push(assistant_msg);
                        messages.push(create_user_message(vec![ContentBlock::Text { text: skill_unused_nudge(&skill) }]));
                        continue;
                    }
                }
                // The reply announces the next step and stops there.
                if config.coding_session && announce_nudges < MAX_ANNOUNCE_NUDGES {
                    let unfinished = if announces_without_acting(&text_content) {
                        Some(("ends on a colon", announce_nudge()))
                    } else if !acted {
                        // Nothing edited or run yet. Files the request names that no call has touched.
                        let unopened: Vec<String> = files_named_in(&original_prompt)
                            .into_iter()
                            .filter(|f| {
                                let base = f.rsplit('/').next().unwrap_or(f);
                                !touched.iter().any(|t| t.contains(base))
                            })
                            .collect();
                        if asks_or_pastes_instead_of_acting(&text_content) {
                            Some(("asks or pastes commands before editing or running anything", no_approval_nudge(&unopened)))
                        } else if !unopened.is_empty() {
                            Some(("stops short of a request that names files it has not opened", no_approval_nudge(&unopened)))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some((why, text)) = unfinished {
                        announce_nudges += 1;
                        info!("[{}] reply {why}, no tool call - sending the agent back ({announce_nudges}/{MAX_ANNOUNCE_NUDGES})", config.agent_id);
                        messages.push(assistant_msg);
                        messages.push(create_user_message(vec![ContentBlock::Text { text }]));
                        continue;
                    }
                }
                // An edit was tried on a file and every try on it was refused: the file is as it was.
                if config.coding_session && unapplied_nudges < MAX_UNAPPLIED_NUDGES {
                    let unchanged = edits.unchanged();
                    if !unchanged.is_empty() {
                        unapplied_nudges += 1;
                        info!("[{}] edits refused on {} - sending the agent back ({unapplied_nudges}/{MAX_UNAPPLIED_NUDGES})", config.agent_id, short_names(&unchanged));
                        messages.push(assistant_msg);
                        messages.push(create_user_message(vec![ContentBlock::Text { text: unapplied_edits_nudge(&unchanged, &edits.last_refusal) }]));
                        continue;
                    }
                }
                // Edited source, ran nothing that checks it.
                if config.coding_session && unverified_edit && verify_nudges < MAX_VERIFY_NUDGES {
                    verify_nudges += 1;
                    let command = suggested_check(&config.working_dir, &messages, last_edit_path.as_deref());
                    info!("[{}] edited source files, ran no check - sending the agent back ({verify_nudges}/{MAX_VERIFY_NUDGES}, command: {command:?})", config.agent_id);
                    messages.push(assistant_msg);
                    messages.push(create_user_message(vec![ContentBlock::Text { text: verify_nudge(verify_nudges, command.as_deref()) }]));
                    continue;
                }
                // The last build/test/lint failed and nothing passed since: do not let "done" through.
                if config.coding_session && !failed_check_nudged {
                    if let Some(cmd) = last_check_failed.clone() {
                        failed_check_nudged = true;
                        info!("[{}] last check failed ({cmd}) and nothing passed since - nudging once", config.agent_id);
                        messages.push(assistant_msg);
                        messages.push(create_user_message(vec![ContentBlock::Text { text: finish_check::failed_check_nudge(&cmd) }]));
                        continue;
                    }
                }
                // Optional: ask a model whether the request is finished. Advisory - an
                // unreadable verdict, or a failed call, lets the agent finish.
                if config.coding_session && config.finish_judge && judge_rounds < 2 && !evidence.is_empty() {
                    judge_rounds += 1;
                    if let Some(v) = run_finish_judge(config, &original_prompt, evidence.lines(), &text_content, cancel).await {
                        if !v.complete && !v.missing.is_empty() {
                            info!("[{}] finish judge: not complete ({} missing) - sending the agent back", config.agent_id, v.missing.len());
                            messages.push(assistant_msg);
                            messages.push(create_user_message(vec![ContentBlock::Text { text: finish_check::judge_feedback(&v.missing) }]));
                            continue;
                        }
                    }
                }
                // A `PreStop` hook (the user's own hooks.json only) may refuse to let the
                // agent stop, up to 3 times a request; its reason goes back as the next
                // user message.
                if stop_gate_rounds < 3 {
                    if let Some(hm) = config.hook_manager.as_ref().filter(|hm| hm.has_hooks_for_event(&HookEvent::PreStop)) {
                        let base = HookInputBase {
                            hook_event_name: HookEvent::PreStop,
                            session_id: config.session_id.clone(),
                            agent_id: config.agent_id.clone(),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            cwd: config.working_dir.clone(),
                        };
                        let res = super::hooks::execute_hooks(
                            hm,
                            &HookEvent::PreStop,
                            &HookInput::Stop(StopInput { base, stop_reason: None }),
                            &super::hooks::ExecuteHooksOptions {
                                client: config.hook_client.as_ref(),
                                profile: config.hook_profile.as_ref(),
                                ..Default::default()
                            },
                        )
                        .await;
                        if res.blocked {
                            stop_gate_rounds += 1;
                            let mut text = res.reason.clone().unwrap_or_else(|| "A Stop hook asked you to keep going.".to_string());
                            for extra in &res.additional_context {
                                text.push('\n');
                                text.push_str(extra);
                            }
                            info!("[{}] PreStop hook refused to let the agent stop ({stop_gate_rounds}/3)", config.agent_id);
                            messages.push(assistant_msg);
                            messages.push(create_user_message(vec![ContentBlock::Text { text }]));
                            continue;
                        }
                    }
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
            chat_jid: &config.chat_jid,
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
                // Only when nothing that could change the answer ran since.
                Some(sig) if seen_tool_sigs.get(&sig) == Some(&epoch) => dup_ids.push(id),
                Some(sig) => {
                    seen_tool_sigs.insert(sig, epoch);
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
        // A new epoch: whatever ran may have changed what an identical call returns.
        if fresh.iter().any(may_change_workspace) {
            epoch += 1;
        }

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
            skill_unused = None;
            if !config.chat_jid.is_empty() {
                let _ = crate::zen_core::session_store::clear(&config.chat_jid);
            }
            messages.push(create_user_message(vec![ContentBlock::Text {
                text: restart_prompt(&original_prompt, plan_content),
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

        // 5c. What the fresh calls did: feeds the stuck detector, the
        //     verify-before-finish tracking and the judge's evidence.
        if !config.is_subagent {
            for tu in &fresh {
                let ContentBlock::ToolUse { id, name, input, .. } = tu else { continue };
                let (content, is_error) = tool_results
                    .iter()
                    .find_map(|r| match r {
                        ContentBlock::ToolResult { tool_use_id, content, is_error } if tool_use_id == id => Some((content.as_str(), *is_error)),
                        _ => None,
                    })
                    .unwrap_or(("", false));
                let failed = finish_check::result_failed(name, content, is_error);
                let label = call_label(name, input);
                if let Some(sig) = tool_call_sig(tu) {
                    detector.record(stuck::Call { sig, result: stuck::fingerprint(content), failed, label: label.clone() });
                }
                evidence.add(&label, failed);
                // A refused ExitPlanMode (an empty plan) asked nobody: Plan mode is not finished,
                // so a text-only reply after it is still sent back for the plan.
                if name == EXIT_PLAN_MODE_TOOL && failed {
                    exit_plan_mode_called = false;
                }
                skill_unused = track_skill_use(skill_unused.take(), name, input, content, failed);
                if config.coding_session {
                    if matches!(name.as_str(), "Edit" | "Write" | "NotebookEdit" | "Bash") {
                        acted = true;
                    }
                    if matches!(name.as_str(), "Edit" | "Write" | "NotebookEdit") {
                        let target = input.get("file_path").or_else(|| input.get("notebook_path")).and_then(|v| v.as_str()).unwrap_or("");
                        edits.record(target, failed, content);
                    }
                    if touched.len() < 200 {
                        touched.push(input.to_string().chars().take(2000).collect());
                    }
                    if is_source_edit(name, input) {
                        if !failed {
                            unverified_edit = true;
                            last_edit_path = input.get("file_path").or_else(|| input.get("notebook_path")).and_then(|v| v.as_str()).map(String::from);
                        }
                    } else if name == "Bash" {
                        if let Some(cmd) = input.get("command").and_then(|v| v.as_str()).filter(|c| is_verification_command(c)) {
                            unverified_edit = false;
                            last_check_failed = failed.then(|| cmd.to_string());
                        }
                    }
                }
            }
        }

        // 6. Stall detection: count consecutive turns that call the same
        //    tool(s) and emit no user-facing text. A weak model can spin on
        //    e.g. browser_search forever (observed: 6+ identical search turns,
        //    context ballooning 7k→72k tokens). Nudge once, then hard-stop.
        let sig = tool_names_sig(&tool_uses);
        // A coding session reads eight files in a row without commenting; the
        // calls differ, and the stuck detector judges them by their results.
        if !config.coding_session && text_content.trim().is_empty() && Some(&sig) == last_sig.as_ref() {
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

        let stuck_now = if config.coding_session && !config.is_subagent { detector.check() } else { None };
        let recently_nudged = stuck_nudge_turn.map_or(false, |t| turn.saturating_sub(t) < STUCK_FORGIVE_TURNS);
        let stuck_hard_stop = stuck_now.is_some() && recently_nudged;
        let stuck_nudge_text = stuck_now.as_ref().filter(|_| !recently_nudged).map(|s| s.nudge());

        let stall_hard_stop = stall_streak >= stall_limit * 2;
        let should_nudge = !nudged && stall_streak >= stall_limit;
        let error_hard_stop = error_streak >= stall_limit * 2;
        let error_should_nudge = !error_nudged && error_streak >= stall_limit;
        let hard_stop = stall_hard_stop || error_hard_stop || stuck_hard_stop;

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
        } else if stuck_hard_stop {
            let what = stuck_now.as_ref().map(|s| s.describe()).unwrap_or_default();
            // Like the token budget, no SessionError: it would end the request before the
            // honest final answer this stop exists to get.
            warn!("[{}] stuck again after the nudge ({what}) - forcing an honest final answer (tools disabled)", config.agent_id);
        }
        // Prefer the error-specific final nudge (which forbids fabricating
        // results) when we stopped because of a tool-error loop.
        let final_nudge = if error_hard_stop {
            TOOL_ERROR_FINAL_NUDGE
        } else if stuck_hard_stop {
            stuck::FINAL_NUDGE
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
            } else if let Some(text) = stuck_nudge_text.filter(|_| !should_nudge) {
                stuck_nudge_turn = Some(turn);
                detector.clear();
                info!("[{}] stuck by results ({}) - nudging to change approach", config.agent_id, stuck_now.as_ref().map(|s| s.describe()).unwrap_or_default());
                blocks.push(ContentBlock::Text { text });
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
        let out = outgoing_messages(&messages, false, &sample_status_input(), None);
        assert!(matches!(out, Cow::Borrowed(_)), "off must never clone");
        assert_eq!(out.len(), messages.len());
    }

    #[test]
    fn turn_context_alone_adds_one_block_to_the_last_user_message_and_nothing_else() {
        let messages = history_ending_in_tool_result();
        let before_len = messages.len();
        let before_last = messages.last().unwrap().message.content.len();
        let out = outgoing_messages(
            &messages,
            false,
            &sample_status_input(),
            Some("<memory_context>\n[1] (fact) 2026-03\nEmily moved to Hue\n</memory_context>"),
        );
        assert!(matches!(out, Cow::Owned(_)));
        assert_eq!(out.len(), before_len, "no message is added, only a block");
        let last = out.last().unwrap();
        assert_eq!(last.message.role, "user");
        assert_eq!(last.message.content.len(), before_last + 1);
        match last.message.content.last().unwrap() {
            ContentBlock::Text { text } => assert!(text.contains("Emily moved to Hue")),
            other => panic!("expected a trailing Text block, got {other:?}"),
        }
        // Nothing agent-status-shaped appears unless that switch is on.
        assert!(!format!("{:?}", out.last().unwrap().message.content).contains("agent_status"));
    }

    #[test]
    fn turn_context_never_reaches_the_persisted_history() {
        // The reason for the whole mechanism: recall is shown to the model on
        // this call and then gone, so it cannot pile up turn after turn.
        let messages = history_ending_in_tool_result();
        let snapshot = format!("{messages:?}");
        let _ = outgoing_messages(&messages, true, &sample_status_input(), Some("recalled"));
        assert_eq!(format!("{messages:?}"), snapshot);
    }

    #[test]
    fn blank_turn_context_is_no_context_and_costs_no_clone() {
        let messages = history_ending_in_tool_result();
        for blank in [None, Some(""), Some("  \n ")] {
            let out = outgoing_messages(&messages, false, &sample_status_input(), blank);
            assert!(matches!(out, Cow::Borrowed(_)), "{blank:?} must borrow");
        }
    }

    #[test]
    fn recall_comes_before_the_status_line_when_both_are_on() {
        let messages = history_ending_in_tool_result();
        let out = outgoing_messages(&messages, true, &sample_status_input(), Some("recalled text"));
        let content = &out.last().unwrap().message.content;
        let n = content.len();
        assert!(matches!(&content[n - 2], ContentBlock::Text { text } if text == "recalled text"));
        assert!(matches!(&content[n - 1], ContentBlock::Text { text } if text.starts_with("<agent_status>")));
    }

    #[test]
    fn agent_status_on_appends_exactly_one_trailing_block_to_the_last_user_message() {
        // (b): the outgoing request carries exactly one trailing status block.
        let messages = history_ending_in_tool_result();
        let before_last_len = messages.last().unwrap().message.content.len();
        let out = outgoing_messages(&messages, true, &sample_status_input(), None);
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
        let _ = outgoing_messages(&messages, true, &sample_status_input(), None);
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
        let _ = outgoing_messages(&messages, true, &sample_status_input(), None);
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
        // Need >= 3 messages and input >= 80% of context (102_400 of 128k).
        let below = vec![make(10), make(20), make(90_000)];
        assert!(!needs_auto_compact(&below, 128_000));
        let above = vec![make(10), make(20), make(105_000)];
        assert!(needs_auto_compact(&above, 128_000));
        // Too few messages never compacts.
        assert!(!needs_auto_compact(&[make(200_000)], 128_000));
    }

    #[test]
    fn compact_messages_keeps_tool_pairs_intact() {
        let mut msgs = Vec::new();
        msgs.push(create_user_message(vec![ContentBlock::Text {
            text: "first".into(),
        }]));
        // Pad with enough filler so keep-recent kicks in.
        for i in 0..20 {
            msgs.push(create_user_message(vec![ContentBlock::Text {
                text: format!("pad-{i}"),
            }]));
            msgs.push(Message {
                msg_type: "assistant".into(),
                message: MessagePayload {
                    role: "assistant".into(),
                    content: vec![ContentBlock::Text {
                        text: format!("ok-{i}"),
                    }],
                },
                uuid: format!("a-{i}"),
                usage: None,
            });
        }
        // Tail: assistant tool_use + user tool_result that must stay paired.
        msgs.push(Message {
            msg_type: "assistant".into(),
            message: MessagePayload {
                role: "assistant".into(),
                content: vec![ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                }],
            },
            uuid: "a-tool".into(),
            usage: None,
        });
        msgs.push(create_user_message(vec![ContentBlock::ToolResult {
            tool_use_id: "call-1".into(),
            content: "ok".into(),
            is_error: false,
        }]));
        msgs.push(create_user_message(vec![ContentBlock::Text {
            text: "latest".into(),
        }]));

        assert!(compact_messages(&mut msgs));
        // tool_result must still have its matching tool_use in the list.
        let uses: std::collections::HashSet<_> = msgs
            .iter()
            .flat_map(|m| tool_use_ids_in(m))
            .collect();
        for m in &msgs {
            for b in &m.message.content {
                if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                    assert!(
                        uses.contains(tool_use_id),
                        "orphan tool_result {tool_use_id} after compact"
                    );
                }
            }
        }
        // Latest real user prompt preserved.
        assert!(msgs.iter().any(|m| {
            m.message.content.iter().any(|b| matches!(
                b,
                ContentBlock::Text { text } if text == "latest"
            ))
        }));
    }

    #[test]
    fn append_continuity_reminder_inserts_system_reminder() {
        let mut msgs = vec![create_user_message(vec![ContentBlock::Text {
            text: "hi".into(),
        }])];
        append_continuity_reminder(&mut msgs, "");
        assert_eq!(msgs.len(), 2);
        let text = match &msgs[1].message.content[0] {
            ContentBlock::Text { text } => text.as_str(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("<system-reminder>"));
        assert!(text.contains("Today's date"));
    }

    fn history_with_bash(command: &str) -> Vec<Message> {
        vec![
            create_user_message(vec![ContentBlock::Text { text: "make it work".into() }]),
            Message {
                msg_type: "assistant".into(),
                message: MessagePayload {
                    role: "assistant".into(),
                    content: vec![ContentBlock::ToolUse { id: "t1".into(), name: "Bash".into(), input: serde_json::json!({ "command": command }) }],
                },
                uuid: "u1".into(),
                usage: None,
            },
        ]
    }

    #[test]
    fn a_model_the_owner_runs_gets_the_long_turn_budget_a_public_host_the_short_one() {
        for (adapt, provider, url) in [
            ("openai", "custom", "http://192.168.0.93:1234/v1"),
            ("openai", "local", "http://127.0.0.1:18788/api/runtimes/models/m/v1"),
            ("openai", "custom", "http://my-mac.local:1234/v1"),
            ("local-mlx", "custom", "https://example.com"),
            ("openai", "app:mlx", "https://example.com"),
        ] {
            assert_eq!(turn_timeout_for(adapt, provider, url), LLM_TURN_TIMEOUT_LOCAL, "{adapt} {provider} {url}");
        }
        for url in ["https://api.deepseek.com", "http://192.168.0.1.evil.com/v1", ""] {
            assert_eq!(turn_timeout_for("openai", "custom", url), LLM_TURN_TIMEOUT, "{url}");
        }
    }

    #[test]
    fn a_restart_after_clear_context_carries_the_request_and_the_plan() {
        let p = restart_prompt("Phát triển app ToDo bằng react typescript và ant-design", "1. Tạo dự án Vite\n2. Thêm antd");
        assert!(p.contains("Phát triển app ToDo bằng react typescript và ant-design"), "the request, as written: {p}");
        assert!(p.contains("1. Tạo dự án Vite") && p.contains("2. Thêm antd"));
        assert!(p.starts_with("(Automatic message"), "a small model must not read the wrapper as the person's words");
        assert!(p.contains("language of the request"));
        assert!(!p.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)), "no Chinese stub in a Vietnamese chat: {p}");
    }

    #[test]
    fn a_restart_with_an_empty_plan_still_has_a_task() {
        // The case that sent a ToDo request off to look up gold prices: the plan was empty and the
        // restart message held nothing else.
        let p = restart_prompt("Make a todo app", "  \n ");
        assert!(p.contains("Make a todo app"));
        assert!(p.contains("none was written down"), "{p}");
        assert!(p.contains("is the task"));
        // No request either: the wrapper alone, never a half-built section.
        let bare = restart_prompt("", "Step 1");
        assert!(!bare.contains("The person's request"));
        assert!(bare.contains("Step 1"));
    }

    #[test]
    fn a_restart_keeps_a_long_request_bounded() {
        // A character the wrapper text never uses, so the count is the request's alone.
        let long = "Ж".repeat(RESTART_REQUEST_MAX_CHARS * 2);
        let p = restart_prompt(&long, "plan");
        assert_eq!(p.matches('Ж').count(), RESTART_REQUEST_MAX_CHARS);
    }

    #[test]
    fn a_skill_that_names_tools_is_unused_until_a_tool_is_called() {
        let skill_text = "# Skill Activated: web-research\nCall mcp__browser__search { \"query\": \"...\" } then read the pages.";
        let load = serde_json::json!({ "skill": "web-research" });
        // Loading it arms the check.
        let armed = track_skill_use(None, "Skill", &load, skill_text, false);
        assert_eq!(armed.as_deref(), Some("web-research"));
        // Looking tools up, planning and asking do not count as using it.
        for meta in ["ToolSearch", "TodoWrite", "AskUserQuestion", "ExitPlanMode"] {
            assert_eq!(track_skill_use(armed.clone(), meta, &serde_json::json!({}), "ok", false).as_deref(), Some("web-research"), "{meta}");
        }
        // A real call - even one that failed - ends it.
        assert_eq!(track_skill_use(armed.clone(), "mcp__core__browser_search", &serde_json::json!({}), "[]", false), None);
        assert_eq!(track_skill_use(armed.clone(), "mcp__core__browser_search", &serde_json::json!({}), "Error: no browser", true), None);
        assert_eq!(track_skill_use(armed, "Bash", &serde_json::json!({}), "", false), None);
    }

    #[test]
    fn a_skill_with_no_tools_in_it_arms_nothing() {
        // A skill that is only instructions (a writing style) has its answer as its work.
        let load = serde_json::json!({ "skill": "tone-of-voice" });
        assert_eq!(track_skill_use(None, "Skill", &load, "# Skill Activated: tone-of-voice\nWrite short sentences.", false), None);
        // A load that failed arms nothing and leaves an earlier one alone.
        let text = "mcp__browser__search";
        assert_eq!(track_skill_use(None, "Skill", &load, text, true), None);
        assert_eq!(track_skill_use(Some("web-research".into()), "Skill", &load, "no tools here", false).as_deref(), Some("web-research"));
    }

    #[test]
    fn the_skill_check_says_what_happened_in_the_language_of_the_request() {
        let nudge = skill_unused_nudge("web-research");
        assert!(nudge.starts_with("(Automatic check - not a message from the user"), "{nudge}");
        assert!(nudge.contains("`web-research`") && nudge.contains("from memory"));
        let vi = skill_unused_warning("web-research", true);
        let en = skill_unused_warning("web-research", false);
        assert!(vi.contains("Chưa tra cứu thật") && vi.contains("web-research"));
        assert!(en.contains("Not looked up") && en.contains("web-research"));
    }

    #[test]
    fn a_reply_that_stops_on_a_colon_announced_something_and_did_nothing() {
        for t in [
            "Đây là nội dung cho file `inventory/service.py`:",
            "Here is the file:\n\n",
            "**Next, the service layer:**",
            "Tiếp theo là phần cli：",
            "Now I will write it _:_",
        ] {
            assert!(announces_without_acting(t), "{t:?}");
        }
        for t in [
            "",
            "Đã xong: tất cả 8 test pass.",
            "All tests pass.",
            "Did it work?",
            "```python\nprint(1)\n```",
            "Kết quả:\n- 8 test pass",
        ] {
            assert!(!announces_without_acting(t), "{t:?}");
        }
        let n = announce_nudge();
        assert!(n.starts_with(stuck::AUTOMATIC) && n.contains("makes no tool call"), "{n}");
    }

    #[test]
    fn a_question_or_a_block_of_code_with_no_tool_call_yet_is_asking_instead_of_acting() {
        for t in [
            "Bạn đồng ý với kế hoạch này chứ?",
            "Shall I go ahead?  \n",
            "Bạn muốn tôi bắt đầu không？",
            "**Do you agree?**",
            "Bước 1:\n```bash\nread_file \"inventory/models.py\"\n```",
        ] {
            assert!(asks_or_pastes_instead_of_acting(t), "{t:?}");
        }
        for t in ["", "Đã xong, 8 test pass.", "I updated models.py and service.py.", "Kết quả:\n- ok", "See `foo.py` for details"] {
            assert!(!asks_or_pastes_instead_of_acting(t), "{t:?}");
        }
        let n = no_approval_nudge(&[]);
        assert!(n.starts_with(stuck::AUTOMATIC) && n.contains("edited and run nothing yet") && n.contains("do not ask for approval"), "{n}");
        assert!(!n.contains("have not opened"), "no files, nothing to name");
    }

    #[test]
    fn the_files_a_request_names_are_found_and_words_are_not_files() {
        let req = "Thêm trường category cho Product. Cập nhật inventory/models.py, inventory/storage.py (vẫn đọc được file JSON cũ), \
                   inventory/service.py và inventory/cli.py, sửa README.md; xem `tests/test_service.py`, config.toml. Chạy `python3 -m unittest discover` e.g. v1.2 hoặc example.com.";
        assert_eq!(
            files_named_in(req),
            vec!["inventory/models.py", "inventory/storage.py", "inventory/service.py", "inventory/cli.py", "README.md", "tests/test_service.py"],
            "capped at six, in order, no words"
        );
        assert!(files_named_in("fix the login bug, see src/a.rs:12 and src/a.rs").contains(&"src/a.rs".to_string()));
        assert_eq!(files_named_in("fix the login bug, e.g. v1.2 on example.com or 3.14"), Vec::<String>::new());
        assert_eq!(files_named_in(""), Vec::<String>::new());
        let n = no_approval_nudge(&["a.py".to_string(), "b.py".to_string()]);
        assert!(n.contains("The request names a.py, b.py, and you have not opened them."), "{n}");
        assert!(no_approval_nudge(&["a.py".to_string()]).contains("and you have not opened it."));
    }

    #[test]
    fn a_file_whose_every_edit_was_refused_is_reported_unchanged_and_one_success_clears_it() {
        let mut t = EditTally::default();
        assert!(t.unchanged().is_empty(), "nothing tried, nothing unchanged");
        t.record("/p/inventory/models.py", true, "Error: old_string not found in models.py.\n  The closest text is lines 3-5.");
        t.record("inventory/models.py", true, "Error: old_string not found in models.py.");
        t.record("/p/inventory/service.py", false, "Edited service.py");
        t.record("/p/inventory/cli.py", true, "Error: old_string not found in cli.py.");
        assert_eq!(t.unchanged(), vec!["/p/inventory/models.py", "/p/inventory/cli.py"], "service.py was changed; models.py is one file however it was spelled");
        assert!(t.last_refusal.starts_with("Error: old_string not found in cli.py"), "{}", t.last_refusal);
        // A later Write that replaces the file clears it, whichever way the path is spelled.
        t.record("./inventory/models.py", false, "Wrote models.py");
        assert_eq!(t.unchanged(), vec!["/p/inventory/cli.py"]);
        t.record("/p/inventory/cli.py", false, "Edited cli.py");
        assert!(t.unchanged().is_empty());

        assert!(same_file("models.py", "/a/b/models.py") && same_file("./a/models.py", "/x/a/models.py"));
        assert!(!same_file("models.py", "/a/b/xmodels.py") && !same_file("a/models.py", "b/models.py"));

        let one = vec!["/p/inventory/models.py".to_string()];
        let two = vec!["/p/inventory/models.py".to_string(), "cli.py".to_string()];
        let n = unapplied_edits_nudge(&one, "Error: old_string not found");
        assert!(n.starts_with(stuck::AUTOMATIC) && n.contains("tried on models.py was refused, so that file is not changed") && n.contains("The last refusal said: \"Error: old_string not found\""), "{n}");
        assert!(unapplied_edits_nudge(&two, "").contains("on models.py, cli.py was refused, so those files are not changed") && !unapplied_edits_nudge(&two, "").contains("last refusal"));
        assert!(unapplied_edits_warning(&two, true).starts_with("⚠️ Chưa áp dụng được thay đổi nào cho: models.py, cli.py."));
        assert!(unapplied_edits_warning(&one, false).starts_with("⚠️ No change was applied to: models.py."));
        let mut long = EditTally::default();
        long.record("a.py", true, &"x ".repeat(500));
        assert!(long.last_refusal.chars().count() <= 300);
    }

    #[test]
    fn the_verify_note_names_the_call_to_make_and_says_the_edits_came_after_the_last_run() {
        let first = verify_nudge(1, Some("python3 -m unittest"));
        assert!(first.starts_with(stuck::AUTOMATIC) && first.contains("`python3 -m unittest`"), "{first}");
        assert!(first.contains("after your last check"), "a model that ran a check last request argues it still counts");
        let second = verify_nudge(2, Some("python3 -m unittest"));
        assert!(second.starts_with(stuck::AUTOMATIC) && second.contains("call the Bash tool now with `python3 -m unittest`"), "{second}");
        // Nothing to name: still a complete note, and the second round still insists.
        assert!(verify_nudge(1, None).contains("narrowest build, test or lint"));
        assert!(verify_nudge(2, None).contains("call the Bash tool now"));
        assert!(!verify_nudge(1, None).contains("<project_commands>"), "nothing was detected, so there is no such block");
    }

    #[test]
    fn the_command_named_is_one_that_ran_in_this_folder_before_one_the_files_suggest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test_a.py"), "import unittest\n").unwrap();
        let wd = dir.path().to_str().unwrap();
        // Earlier in the conversation (a previous request of the session): that exact call.
        let history = history_with_bash("python3 -m unittest test_a.py");
        assert_eq!(suggested_check(wd, &history, None).as_deref(), Some("python3 -m unittest test_a.py"));
        // A Bash call that checks nothing is not a check.
        let history = history_with_bash("ls -la");
        assert_eq!(last_check_command(&history), None);
        assert_eq!(suggested_check(wd, &history, None).as_deref(), Some("python3 -m unittest"), "falls back to what the folder's files support");
        // Nothing in the folder says how to check: no name rather than a guess.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(suggested_check(empty.path().to_str().unwrap(), &[], None), None);
    }

    #[test]
    fn the_stack_that_holds_the_edited_file_is_the_one_named() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("server")).unwrap();
        std::fs::create_dir_all(dir.path().join("web")).unwrap();
        std::fs::write(dir.path().join("server/Cargo.toml"), "[package]\nname = \"s\"\n").unwrap();
        std::fs::write(dir.path().join("web/package.json"), "{\"scripts\":{\"test\":\"vitest\"}}").unwrap();
        let wd = dir.path().to_str().unwrap();
        let edited = format!("{wd}/web/src/a.ts");
        let named = suggested_check(wd, &[], Some(&edited)).unwrap();
        assert!(named.contains("web") && named.contains("test"), "{named}");
        let named = suggested_check(wd, &[], Some(&format!("{wd}/server/src/lib.rs"))).unwrap();
        assert!(named.contains("server") && named.contains("cargo"), "{named}");
    }

    #[test]
    fn the_warning_follows_the_language_of_the_request_and_names_the_check() {
        let vi = unverified_warning(Some("python3 -m unittest"), true);
        assert!(vi.starts_with("⚠️ Chưa kiểm chứng") && vi.contains("`python3 -m unittest`"), "{vi}");
        let en = unverified_warning(Some("cargo test"), false);
        assert!(en.starts_with("⚠️ Not verified") && en.contains("`cargo test`"), "{en}");
        assert!(unverified_warning(None, false).contains("Run the tests or the build"));
        assert!(unverified_warning(None, true).contains("tự chạy test"));
    }

    #[test]
    fn only_source_edits_ask_for_a_check() {
        let edit = |tool: &str, p: &str| is_source_edit(tool, &serde_json::json!({"file_path": p}));
        assert!(edit("Edit", "/p/src/main.rs"));
        assert!(edit("Write", "/p/app/page.tsx"));
        assert!(edit("Edit", "/p/lib/main.dart"));
        assert!(!edit("Edit", "/p/README.md"));
        assert!(!edit("Write", "/p/notes.txt"));
        assert!(!edit("Read", "/p/src/main.rs"), "reading is not editing");
    }

    #[test]
    fn any_build_test_or_lint_counts_as_a_check() {
        for c in [
            "cargo test --lib x", "cd web && npm run build", "pnpm run typecheck", "python3 -m pytest -q", "go test ./...", "flutter analyze", "make test",
            "python3 check.py", "node test.js", "./scripts/verify.sh", "bash scripts/check.sh", "make", "FOO=1 python3 x.py", "cd web && node build.js",
        ] {
            assert!(is_verification_command(c), "{c}");
        }
        for c in ["ls -la", "git status", "cat src/main.rs", "mkdir -p x", "git commit -m x", "python3", "echo hi && ls", "rm -rf build"] {
            assert!(!is_verification_command(c), "{c}");
        }
    }

    #[test]
    fn only_workspace_changing_calls_start_a_new_epoch() {
        let call = |name: &str, input: serde_json::Value| tool_use(name, input);
        assert!(may_change_workspace(&call("Edit", serde_json::json!({"file_path": "a"}))));
        assert!(may_change_workspace(&call("Write", serde_json::json!({}))));
        assert!(may_change_workspace(&call("Bash", serde_json::json!({"command": "cargo test"}))));
        assert!(may_change_workspace(&call("Bash", serde_json::json!({}))), "no command shown: assume it changes things");
        assert!(!may_change_workspace(&call("Bash", serde_json::json!({"command": "git status"}))));
        assert!(!may_change_workspace(&call("Read", serde_json::json!({"file_path": "a"}))));
        assert!(!may_change_workspace(&call("Grep", serde_json::json!({"pattern": "x"}))));
        assert!(!may_change_workspace(&ContentBlock::Text { text: "x".into() }));
    }

    #[test]
    fn a_call_is_labelled_the_way_a_person_names_it() {
        assert_eq!(call_label("Bash", &serde_json::json!({"command": "cargo   test  --lib"})), "Bash: cargo test --lib");
        assert_eq!(call_label("Edit", &serde_json::json!({"file_path": "/p/src/a.rs"})), "Edit: /p/src/a.rs");
        assert_eq!(call_label("TodoWrite", &serde_json::json!({"todos": []})), "TodoWrite");
        assert!(call_label("Bash", &serde_json::json!({"command": "x".repeat(500)})).len() < 100);
    }

    #[test]
    fn the_request_is_the_last_text_of_the_last_real_user_message() {
        let messages = history_ending_in_tool_result();
        assert_eq!(last_user_prompt(&messages), "please read app.py");
        assert_eq!(last_user_prompt(&[]), "");
    }

    #[test]
    fn the_duplicate_note_no_longer_tells_a_coding_run_to_stop() {
        assert!(DUPLICATE_CALL_NOTE.contains("nothing has been edited"));
        assert!(!DUPLICATE_CALL_NOTE.contains("final answer"), "a retest after an edit is not a reason to finish");
    }
}

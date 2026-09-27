//! Public payload types, event data types, callback aliases, and internal enums.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::agent::permission_bridge::AskQuestionData;

// ===== Constants =====

/// Agent ID emitted by sema-core for the main (root) agent. Subagent events
/// carry a different id and are filtered out.
#[allow(dead_code)] // wired by Phase 2 bind_events
pub(crate) const MAIN_AGENT_ID: &str = "main";

/// `process_and_wait` inactivity timeout (30 minutes). Must exceed the longest
/// dispatch_task runtime so chained tool calls don't trip the watchdog.
pub const AGENT_TIMEOUT_MS: u64 = 30 * 60 * 1000;

// ===== Public payload types =====

/// Permission flags surfaced to the Web UI / virtual workers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PermissionsConfig {
    #[serde(rename = "skipMainAgentPermissions")]
    pub skip_main_agent_permissions: bool,
    #[serde(rename = "skipAllAgentsPermissions")]
    pub skip_all_agents_permissions: bool,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            skip_main_agent_permissions: false,
            skip_all_agents_permissions: false,
        }
    }
}

/// One TodoWrite item snapshot — cached for replay on WS subscribe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoSnapshot {
    pub content: String,
    pub status: String,
    #[serde(
        default,
        rename = "activeForm",
        skip_serializing_if = "Option::is_none"
    )]
    pub active_form: Option<String>,
}

/// Per-agent cached todos snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct CachedTodos {
    #[serde(rename = "agentName")]
    pub agent_name: String,
    pub todos: Vec<TodoSnapshot>,
}

// ===== Event data types (mirrors TS sema-core events) =====

/// `message:complete` event payload.
#[derive(Debug, Clone)]
pub struct MessageCompleteData {
    pub agent_id: String,
    pub reasoning: String,
    pub content: String,
    /// True when the turn produced one or more tool calls (intermediate turn —
    /// no user-facing answer expected; the tool runs next, then a follow-up
    /// turn produces the real answer). Used by `pool::merge_assistant_reasoning_for_web_ui`
    /// to decide whether to surface reasoning as the body (final turn) or keep
    /// it collapsed under `<think>` (intermediate turn).
    pub has_tool_calls: bool,
    /// Output (completion) tokens this assistant message cost. 0 when the
    /// provider didn't report usage. Forwarded to the chat UI per-message.
    pub output_tokens: u32,
}

/// `state:update` event payload.
#[derive(Debug, Clone)]
pub struct StateUpdateData {
    pub state: String,
}

/// `todos:update` event payload — list of todo items.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodosUpdateItem {
    pub content: String,
    pub status: String,
    #[serde(
        default,
        rename = "activeForm",
        skip_serializing_if = "Option::is_none"
    )]
    pub active_form: Option<String>,
}

/// `compact:start` event payload.
#[derive(Debug, Clone)]
pub struct CompactStartData;

/// `compact:exec` event payload.
#[derive(Debug, Clone, Default)]
pub struct CompactExecData {
    /// The LLM compaction summary that replaced the dropped history, when the
    /// compaction produced one (None on keep-recent truncation fallback).
    /// Consumed by the curated-memory consolidation hook in `pool.rs`.
    pub summary: Option<String>,
}

/// `session:error` event payload.
#[derive(Debug, Clone)]
pub struct SessionErrorData {
    pub code: String,
    pub message: String,
}

/// `tool:permission:request` event payload.
#[derive(Debug, Clone)]
pub struct ToolPermissionRequestData {
    pub tool_name: String,
    /// Key an "allow / never ask again" answer must be persisted under
    /// (`Skill(<name>)`, `Bash(<cmd>)`, …). Never `tool_name` — see the
    /// zen_core struct of the same name.
    pub permission_key: String,
    pub title: String,
    pub content: serde_json::Value,
    pub options: HashMap<String, String>,
}

/// `ask:question:request` event payload.
#[derive(Debug, Clone)]
pub struct AskQuestionRequestData {
    pub agent_id: String,
    pub questions: Vec<AskQuestionData>,
}

/// Events forwarded from `bind_events` persistent handlers to an active
/// `process_and_wait` event loop. Sent through the unbounded channel stored
/// in [`State::process_event_txs`].
#[derive(Debug, Clone)]
pub(crate) enum ProcessEvent {
    /// Core reached idle — resolve the PAW promise.
    Idle,
    /// Core emitted a session error — trigger error handling.
    Error(SessionErrorData),
    /// Non-idle, non-paused state update — restart the inactivity timer.
    Reset,
}

// ===== Callback type aliases =====

/// Reply callback (jid, text). Used by the WebSocket gateway path before
/// `set_send_reply` lands.
pub type ReplyFn = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Channel send callback (jid, text, bot_token). Replaces ReplyFn for
/// channel-bound replies once message_router wires it up (Phase 2).
pub type SendReplyFn = Arc<dyn Fn(&str, &str, Option<&str>) + Send + Sync>;

/// Typing indicator callback (jid, active, bot_token).
pub type TypingFn = Arc<dyn Fn(&str, bool, Option<&str>) + Send + Sync>;

/// Inactivity-timer reset closure stored per JID during process_and_wait.
pub(crate) type ActivityResetFn = Arc<dyn Fn() + Send + Sync>;

/// Abort callback stored per JID — invoked on `destroy()` to break a pending
/// process_and_wait promise.
pub(crate) type AbortFn = Box<dyn FnOnce(&str) + Send>;

/// Cleanup callback stored per JID — removes persistent event listeners.
pub(crate) type CleanupFn = Box<dyn FnOnce() + Send>;

/// Workspace-state-file unwatch callback.
pub(crate) type UnwatchFn = Box<dyn FnOnce() + Send>;

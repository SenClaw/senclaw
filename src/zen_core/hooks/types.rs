//! Hook types — mirror of TS sema-core `hooks/types.ts`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// HookEvent
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    /// Fired AFTER permission was granted (notification-only).
    PermissionRequest,
    /// Fired BEFORE the user is prompted for permission. A hook can return
    /// `decision: "allow"` to skip the prompt, or `decision: "reject"` /
    /// `blocked: true` to deny without bothering the user.
    PrePermission,
    /// Fired after a tool returns its output. A hook can return a value in
    /// `updatedOutput` to replace the content the engine emits (used for
    /// redaction / truncation).
    OutputFilter,
    Stop,
    SessionStart,
    SessionEnd,
    PreCompact,
    PostCompact,
    Notification,
    Error,
    SubagentStart,
    SubagentEnd,
}

impl HookEvent {
    /// Events that can never block the main flow even if a hook returns `blocked: true`.
    pub fn is_non_blockable(&self) -> bool {
        matches!(
            self,
            HookEvent::SessionEnd
                | HookEvent::Stop
                | HookEvent::PostToolUse
                | HookEvent::PostCompact
                | HookEvent::SubagentEnd
        )
    }
}

// ============================================================================
// Config
// ============================================================================

/// Top-level hook configuration loaded from `settings.json` / `CLAUDE.md`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookConfig {
    #[serde(default)]
    pub hooks: HashMap<HookEvent, Vec<HookEventConfig>>,
}

/// One entry under a hook event — optional matcher/if filter + list of hooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEventConfig {
    /// Glob pattern matched against the tool name (or notification type).
    /// `*` matches everything. Comma-separated values are OR'd.
    pub matcher: Option<String>,

    /// Regex applied to the serialised `tool_input` (or message text).
    /// Matched with `Regex::is_match()`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,

    pub hooks: Vec<HookDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDefinition {
    #[serde(rename = "type")]
    pub hook_type: HookType,
    pub command: Option<String>,
    pub prompt: Option<String>,
    /// Timeout in seconds (default 10 for command, 30 for prompt).
    pub timeout: Option<u64>,
    /// Whether a non-zero exit / reject decision blocks the main flow.
    /// Defaults to `true`. Forced `false` when `async = true`.
    pub blocking: Option<bool>,
    /// Fire-and-forget — do not await completion.
    /// Prompt hooks are always sync.
    #[serde(rename = "async")]
    pub is_async: Option<bool>,
    /// Include message history in hook input (for prompt hooks).
    /// Defaults to `false`.
    pub include_history: Option<bool>,
    /// Maximum number of recent messages to include in history.
    /// Defaults to 10 when include_history is true.
    pub history_limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookType {
    Command,
    Prompt,
}

impl HookDefinition {
    pub fn normalize(mut self) -> Self {
        if self.hook_type == HookType::Prompt {
            self.is_async = Some(false);
        }
        if self.is_async == Some(true) {
            self.blocking = Some(false);
        }
        if self.blocking.is_none() {
            self.blocking = Some(true);
        }
        if self.is_async.is_none() {
            self.is_async = Some(false);
        }
        if self.include_history.is_none() {
            self.include_history = Some(false);
        }
        if self.history_limit.is_none() {
            self.history_limit = Some(10);
        }
        self
    }

    pub fn is_blocking(&self) -> bool {
        self.blocking.unwrap_or(true)
    }

    pub fn is_fire_and_forget(&self) -> bool {
        self.is_async.unwrap_or(false)
    }
}

// ============================================================================
// HookInput — typed payloads sent to hook processes / prompts
// ============================================================================

/// Base fields present on every hook input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookInputBase {
    pub hook_event_name: HookEvent,
    pub session_id: String,
    pub agent_id: String,
    pub timestamp: String,
    pub cwd: String,
}

/// All concrete hook input variants.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum HookInput {
    UserPromptSubmit(UserPromptSubmitInput),
    PreToolUse(PreToolUseInput),
    PostToolUse(PostToolUseInput),
    PermissionRequest(PermissionRequestInput),
    PrePermission(PrePermissionInput),
    OutputFilter(OutputFilterInput),
    PreCompact(PreCompactInput),
    Session(SessionInput),
    Stop(StopInput),
    Subagent(SubagentInput),
    Notification(NotificationInput),
    Error(ErrorInput),
}

impl HookInput {
    /// Extract the text that the `if` condition regex is matched against.
    pub fn extract_match_text(&self) -> String {
        match self {
            HookInput::PreToolUse(i) => {
                if let Some(cmd) = i.tool_input.get("command").and_then(|v| v.as_str()) {
                    cmd.to_string()
                } else {
                    i.tool_input.to_string()
                }
            }
            HookInput::PostToolUse(i) => {
                if let Some(cmd) = i.tool_input.get("command").and_then(|v| v.as_str()) {
                    cmd.to_string()
                } else {
                    i.tool_input.to_string()
                }
            }
            HookInput::PermissionRequest(i) => {
                if let Some(cmd) = i.tool_input.get("command").and_then(|v| v.as_str()) {
                    cmd.to_string()
                } else {
                    i.tool_input.to_string()
                }
            }
            HookInput::PrePermission(i) => {
                if let Some(cmd) = i.tool_input.get("command").and_then(|v| v.as_str()) {
                    cmd.to_string()
                } else {
                    i.tool_input.to_string()
                }
            }
            HookInput::OutputFilter(i) => {
                if let Some(cmd) = i.tool_input.get("command").and_then(|v| v.as_str()) {
                    cmd.to_string()
                } else {
                    i.tool_input.to_string()
                }
            }
            HookInput::Notification(i) => i.message.clone(),
            HookInput::Error(i) => i.error_message.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        }
    }

    /// Return the tool_name if this is a tool-related event, for matcher matching.
    pub fn match_query(&self) -> Option<&str> {
        match self {
            HookInput::PreToolUse(i) => Some(&i.tool_name),
            HookInput::PostToolUse(i) => Some(&i.tool_name),
            HookInput::PermissionRequest(i) => Some(&i.tool_name),
            HookInput::PrePermission(i) => Some(&i.tool_name),
            HookInput::OutputFilter(i) => Some(&i.tool_name),
            HookInput::Notification(i) => i.notification_type.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UserPromptSubmitInput {
    #[serde(flatten)]
    pub base: HookInputBase,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreToolUseInput {
    #[serde(flatten)]
    pub base: HookInputBase,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct PostToolUseInput {
    #[serde(flatten)]
    pub base: HookInputBase,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_response: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionRequestInput {
    #[serde(flatten)]
    pub base: HookInputBase,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
}

/// Sent BEFORE the user is prompted for permission. Hook returns
/// `{"decision": "allow"}` to skip the prompt, or `{"decision": "reject"}`
/// (or `{"blocked": true}`) to auto-deny. Anything else falls through to
/// the existing permission flow.
#[derive(Debug, Clone, Serialize)]
pub struct PrePermissionInput {
    #[serde(flatten)]
    pub base: HookInputBase,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
}

/// Sent AFTER the tool returns its raw output, BEFORE the engine emits
/// `ToolExecutionComplete`. Hook can return `{"updatedOutput": {...}}`
/// to replace the content (useful for redaction / truncation).
#[derive(Debug, Clone, Serialize)]
pub struct OutputFilterInput {
    #[serde(flatten)]
    pub base: HookInputBase,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub tool_output: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreCompactInput {
    #[serde(flatten)]
    pub base: HookInputBase,
    pub message_count: usize,
    pub context_history: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInput {
    #[serde(flatten)]
    pub base: HookInputBase,
}

#[derive(Debug, Clone, Serialize)]
pub struct StopInput {
    #[serde(flatten)]
    pub base: HookInputBase,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubagentInput {
    #[serde(flatten)]
    pub base: HookInputBase,
    pub subagent_id: String,
    pub subagent_task: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NotificationInput {
    #[serde(flatten)]
    pub base: HookInputBase,
    pub notification_type: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorInput {
    #[serde(flatten)]
    pub base: HookInputBase,
    pub error_message: String,
    pub error_type: Option<String>,
}

// ============================================================================
// HookOutput / AggregatedHookResult
// ============================================================================

/// Response written to stdout by a command hook, or returned by a prompt hook.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookOutput {
    pub decision: Option<String>,
    pub blocked: Option<bool>,
    pub reason: Option<String>,
    pub abort: Option<bool>,
    #[serde(rename = "updatedInput")]
    pub updated_input: Option<serde_json::Value>,
    /// Replacement value emitted by an OutputFilter hook. The engine swaps
    /// the tool's raw output for this before constructing
    /// `ToolExecutionComplete` and the assistant-facing message.
    #[serde(rename = "updatedOutput")]
    pub updated_output: Option<serde_json::Value>,
    #[serde(rename = "additionalContext")]
    pub additional_context: Option<String>,
    pub response: Option<String>,
}

impl HookOutput {
    pub fn is_blocked(&self) -> bool {
        self.blocked.unwrap_or(false) || self.decision.as_deref() == Some("reject")
    }

    pub fn is_abort(&self) -> bool {
        self.abort.unwrap_or(false)
    }

    /// True when a `PrePermission` hook returned `decision: "allow"`.
    pub fn is_allow(&self) -> bool {
        self.decision.as_deref() == Some("allow")
    }
}

#[derive(Debug, Clone)]
pub struct HookError {
    pub hook: HookDefinition,
    pub error: String,
}

/// Aggregated result of running all matching hooks for a single event.
#[derive(Debug, Clone)]
pub struct AggregatedHookResult {
    /// Whether a blocking hook requested the action be blocked.
    pub blocked: bool,
    /// Whether the session should be aborted entirely.
    pub abort: bool,
    /// Human-readable reason from the blocking hook.
    pub reason: Option<String>,
    /// Updated tool input from a hook (PreToolUse only).
    pub updated_input: Option<serde_json::Value>,
    /// Replacement tool output from an OutputFilter hook (the last hook
    /// in the chain to set `updatedOutput` wins).
    pub updated_output: Option<serde_json::Value>,
    /// `true` when any hook explicitly returned `decision: "allow"` —
    /// used by `PrePermission` to grant a tool without prompting.
    pub allow: bool,
    /// Additional context strings to inject into the conversation.
    pub additional_context: Vec<String>,
    pub errors: Vec<HookError>,
}

impl AggregatedHookResult {
    pub fn empty() -> Self {
        Self {
            blocked: false,
            abort: false,
            reason: None,
            updated_input: None,
            updated_output: None,
            allow: false,
            additional_context: Vec::new(),
            errors: Vec::new(),
        }
    }
}

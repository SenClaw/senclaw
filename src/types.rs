//! Shared types. Mirrors `src-old/types.ts`.
//!
//! `IChannel` is a TS interface; in Rust it becomes a trait — left out for now
//! and lands together with the channel module ports.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ===== Channel layer =====

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatType {
    Private,
    Group,
    Supergroup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingMessage {
    pub id: String,
    pub chat_jid: String,
    pub sender_name: String,
    pub sender_jid: String,
    pub content: String,
    pub timestamp: String,
    pub is_from_me: bool,
    pub chat_type: ChatType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mentions_bot_username: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_msg_id: Option<String>,
    /// Media the adapter downloaded for this message (photos, files). Empty for
    /// text-only messages and for adapters that don't fetch media.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<MessageAttachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMeta {
    pub jid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub chat_type: ChatType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineButton {
    pub label: String,
    pub callback_data: String,
}

// ===== Gateway layer =====

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupBinding {
    pub jid: String,
    pub folder: String,
    pub name: String,
    /// "" = web-only (no channel binding).
    pub channel: String,
    /// "chat" | "cowork" | "code"
    pub group_type: String,
    pub requires_trigger: bool,
    /// `None` = all tools allowed.
    pub allowed_tools: Option<Vec<String>>,
    pub allowed_paths: Option<Vec<String>>,
    /// `None` = workspace switching disallowed.
    pub allowed_work_dirs: Option<Vec<String>>,
    /// `None` = use `TELEGRAM_BOT_TOKEN`.
    pub bot_token: Option<String>,
    /// `None` = use `MAX_MESSAGES_PER_GROUP`.
    pub max_messages: Option<u32>,
    /// Per-group LLM override: id of an entry in the global `llmConfigs` list.
    /// `None` = use the globally active model (`activeLlmConfigId`).
    pub llm_config_id: Option<String>,
    pub last_active: Option<String>,
    pub added_at: String,
}

// ===== New entity model =====

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: i64,
    pub platform_type: String,
    pub name: String,
    pub credentials_json: String,
    pub connection_state: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: i64,
    pub folder: String,
    pub name: String,
    pub requires_trigger: bool,
    pub allowed_tools: Option<Vec<String>>,
    pub allowed_paths: Option<Vec<String>>,
    pub allowed_work_dirs: Option<Vec<String>>,
    pub core_prompt: String,
    pub model_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binding {
    pub id: i64,
    /// None = pending binding (auto-complete on first message)
    pub jid: Option<String>,
    pub agent_id: i64,
    pub channel_id: i64,
    pub bot_token_override: Option<String>,
    pub max_messages: Option<u32>,
    pub last_active: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingWithRelations {
    pub binding: Binding,
    pub agent: Agent,
    pub channel: Channel,
}

/// A chat asking to be let in, and the code it was told to quote.
///
/// This is a *request*, never a grant: only `status == "approved"` — set by a
/// human, never by an arriving message — turns into a binding. `sender_jid` and
/// `sender_name` exist so the approver is shown who is knocking rather than a
/// bare chat id, which is the whole point of asking a person.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelPairing {
    pub id: i64,
    pub channel_id: i64,
    pub chat_jid: String,
    /// `"user"` or `"group"` — a group request lets in everyone in that group,
    /// so the UI has to say which kind this is.
    pub chat_type: String,
    pub sender_jid: String,
    pub sender_name: String,
    pub chat_title: Option<String>,
    pub code: String,
    /// `pending` | `approved` | `rejected` | `superseded`
    pub status: String,
    /// Which bot the request arrived through, so approval binds to that bot
    /// rather than whichever one happens to be default at approval time.
    pub bot_token: Option<String>,
    pub created_at: String,
    pub expires_at: String,
    pub resolved_at: Option<String>,
}

// ===== Agent API trait =====

/// A file riding along with a chat message.
///
/// One shape for every producer — the web composer, the desktop picker, and
/// channel adapters that download media — and the shape persisted in
/// [`StoredMessage::attachments`]. Field names are the JSON the clients already
/// send; `name` is absent for pasted images, which have no filename.
///
/// Images and documents share this type deliberately: the routing decision
/// belongs to the agent pool (which knows the model's capabilities), not to
/// whichever surface happened to accept the upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAttachment {
    /// `data:<mime>;base64,…`, an http(s) URL, or an absolute local path.
    #[serde(rename = "dataUrl")]
    pub data_url: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    /// Original filename, when the source had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl MessageAttachment {
    /// Whether this attachment takes the image route (vision blocks or OCR)
    /// rather than the document route (text extraction).
    pub fn is_image(&self) -> bool {
        self.mime_type.starts_with("image/")
    }

    /// Filename to show the model, falling back to a generic label.
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("(unnamed)")
    }
}

/// Operations that the message router and cowork manager need from the agent pool.
#[async_trait]
pub trait AgentApi: Send + Sync {
    /// Send a direct reply to a chat (for admin commands and unregistered notices).
    async fn broadcast_reply(&self, chat_jid: &str, text: &str, bot_token: Option<&str>);

    /// Process a prompt through the agent. Blocks until the agent finishes.
    async fn process_and_wait(&self, jid: &str, group: &GroupBinding, prompt: &str) -> Result<()>;

    /// Process a prompt with attachments through the agent. Blocks until the
    /// agent finishes.
    async fn process_and_wait_with_attachments(
        &self,
        jid: &str,
        group: &GroupBinding,
        prompt: &str,
        _attachments: &[MessageAttachment],
    ) -> Result<()> {
        // Default implementation: ignore attachments and call the basic version
        self.process_and_wait(jid, group, prompt).await
    }

    /// Destroy/cleanup agent state for a JID (after JID migration).
    async fn destroy(&self, jid: &str);

    /// Return the last assistant reply text produced during `process_and_wait`
    /// for `jid`. Used to persist task results. Default returns `None`.
    fn get_last_reply_text(&self, _jid: &str) -> Option<String> {
        None
    }
}

/// No-op stub — used before AgentPool is ported or when agent execution is unavailable.
pub struct NoopAgentApi;

#[async_trait]
impl AgentApi for NoopAgentApi {
    async fn broadcast_reply(&self, _jid: &str, _text: &str, _token: Option<&str>) {}
    async fn process_and_wait(
        &self,
        _jid: &str,
        _group: &GroupBinding,
        _prompt: &str,
    ) -> Result<()> {
        Ok(())
    }
    async fn destroy(&self, _jid: &str) {}
}

// ===== DB layer =====

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub message_id: String,
    pub chat_jid: String,
    pub sender_jid: String,
    pub sender_name: String,
    pub content: String,
    pub timestamp: String,
    pub is_from_me: bool,
    pub is_bot_reply: bool,
    pub reply_to_id: Option<String>,
    pub media_type: Option<String>,
    /// JSON-serialized `Vec<MessageAttachment>`.
    pub attachments: Option<String>,
}

impl StoredMessage {
    /// Parse [`Self::attachments`], tolerating rows written before the column
    /// existed and rows a future client wrote in a shape we can't read: history
    /// must keep loading either way.
    pub fn parsed_attachments(&self) -> Vec<MessageAttachment> {
        let Some(raw) = self.attachments.as_deref().map(str::trim) else {
            return Vec::new();
        };
        if raw.is_empty() {
            return Vec::new();
        }
        match serde_json::from_str::<Vec<MessageAttachment>>(raw) {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(
                    "[StoredMessage] unreadable attachments on {}: {e}",
                    self.message_id
                );
                Vec::new()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Active,
    Paused,
    Completed,
    Error,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Error => "error",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "error" => Self::Error,
            _ => Self::Active,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleType {
    Cron,
    Interval,
    Once,
    /// Fire exactly once (like [`Once`]) and then delete the task row entirely,
    /// instead of leaving it behind with `status = completed`. `schedule_value`
    /// is an ISO-8601 timestamp, same as `Once`.
    #[serde(rename = "once_delete")]
    OnceDelete,
}

impl ScheduleType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cron => "cron",
            Self::Interval => "interval",
            Self::Once => "once",
            Self::OnceDelete => "once_delete",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "interval" => Self::Interval,
            "once" => Self::Once,
            // Accept a couple of spellings so callers don't have to guess.
            "once_delete" | "once-delete" | "oncedelete" => Self::OnceDelete,
            _ => Self::Cron,
        }
    }

    /// True for one-shot schedule types (`once` and `once_delete`) that never
    /// produce a subsequent `next_run`.
    pub fn is_one_shot(self) -> bool {
        matches!(self, Self::Once | Self::OnceDelete)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextMode {
    Isolated,
    Group,
    Notify,
    Script,
    ScriptAgent,
    /// Re-check a condition on an interval and dispatch into the chat only once
    /// it holds. See [`crate::scheduler::watch`].
    Watch,
}

impl ContextMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Isolated => "isolated",
            Self::Group => "group",
            Self::Notify => "notify",
            Self::Script => "script",
            Self::ScriptAgent => "script-agent",
            Self::Watch => "watch",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "group" => Self::Group,
            "notify" => Self::Notify,
            "script" => Self::Script,
            "script-agent" => Self::ScriptAgent,
            "watch" => Self::Watch,
            _ => Self::Isolated,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Agent,
    Dag,
    Plan,
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Dag => "dag",
            Self::Plan => "plan",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "dag" => Self::Dag,
            "plan" => Self::Plan,
            _ => Self::Agent,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Success,
    Error,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "error" => Self::Error,
            _ => Self::Success,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub group_folder: String,
    pub chat_jid: String,
    pub prompt: String,
    pub schedule_type: ScheduleType,
    pub schedule_value: String,
    pub context_mode: ContextMode,
    pub agent_mode: AgentMode,
    /// Bash command for `Script` / `ScriptAgent` modes.
    pub script_command: Option<String>,
    /// Serialised [`crate::scheduler::watch::WatchConfig`] for `Watch` mode.
    /// One JSON column because every field in it is meaningless in any other
    /// mode, and only the watch module reads or writes it.
    pub watch_json: Option<String>,
    pub next_run: Option<String>,
    pub last_run: Option<String>,
    pub last_result: Option<String>,
    pub status: TaskStatus,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRunLog {
    pub id: i64,
    pub task_id: String,
    pub run_at: String,
    pub duration_ms: Option<i64>,
    pub status: RunStatus,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TaskRunLogInsert {
    pub task_id: String,
    pub run_at: String,
    pub duration_ms: Option<i64>,
    pub status: RunStatus,
    pub result: Option<String>,
    pub error: Option<String>,
}

// ===== Background tasks =====
//
// Autonomous work SenClaw runs by itself: no chat session, no reply, no user
// waiting. Deliberately kept apart from [`ScheduledTask`] above, which is the
// *user's* schedule and assumes a human on the other end (see
// `docs/background-tasks-design.md` §1).

/// Who declared a background task. Decides what may edit it: an App's tasks
/// live in its manifest and a native job's body is Rust, so neither is
/// editable through the API — only pausable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundOwnerKind {
    /// Core upkeep (`owner_id` = `core.cognitive`, …).
    System,
    /// A Space App (`owner_id` = the app id).
    App,
    /// A chat or the UI (`owner_id` = the agent folder).
    User,
}

impl BackgroundOwnerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::App => "app",
            Self::User => "user",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "system" => Self::System,
            "app" => Self::App,
            _ => Self::User,
        }
    }
    /// True when the API may edit or delete the task. App-owned tasks are
    /// reverted by a reinstall; native jobs have no prompt to edit.
    pub fn is_editable(self) -> bool {
        matches!(self, Self::User)
    }
}

/// A task body is either a prompt run through the agent, or a Rust closure
/// registered in the native registry (existing upkeep loops, brought under the
/// same run history / statistics / pause surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundJobKind {
    Prompt,
    Native,
}

impl BackgroundJobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Native => "native",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "native" => Self::Native,
            _ => Self::Prompt,
        }
    }
}

/// How the prompt for a run is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundPromptKind {
    /// `prompt` verbatim.
    Static,
    /// GET `context_url` → JSON, substitute `{{var}}` into `prompt`. Empty
    /// context skips the run, so a task with nothing to do costs no tokens.
    Template,
    /// One `llm.request` with `prompt` as the instruction; its output becomes
    /// the real prompt. Doubles token cost — prefer `Template`.
    Generator,
}

impl BackgroundPromptKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Template => "template",
            Self::Generator => "generator",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "template" => Self::Template,
            "generator" => Self::Generator,
            _ => Self::Static,
        }
    }
}

/// When a task fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTrigger {
    /// `trigger_value` is a cron expression (5- or 6-field), local timezone.
    Cron,
    /// `trigger_value` is milliseconds.
    Interval,
    /// `trigger_value` is an RFC3339 timestamp.
    Once,
    /// Fires once when the owning App is installed, then never again.
    OnInstall,
    /// Never fires on its own; `run_now` only.
    Manual,
}

impl BackgroundTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cron => "cron",
            Self::Interval => "interval",
            Self::Once => "once",
            Self::OnInstall => "on_install",
            Self::Manual => "manual",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "interval" => Self::Interval,
            "once" => Self::Once,
            "on_install" | "on-install" | "oninstall" => Self::OnInstall,
            "manual" => Self::Manual,
            _ => Self::Cron,
        }
    }
    /// True for triggers that never produce a subsequent `next_run`.
    pub fn is_one_shot(self) -> bool {
        matches!(self, Self::Once | Self::OnInstall | Self::Manual)
    }
}

/// What to do when a task is still running as its next window arrives. A
/// 5-minute task on a 1-minute interval is a real configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapPolicy {
    /// Record a `skipped` run and move on.
    Skip,
    /// Wait for the in-flight run to finish.
    Queue,
    /// Cancel the in-flight run and start fresh.
    CancelPrevious,
}

impl OverlapPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Queue => "queue",
            Self::CancelPrevious => "cancel_previous",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "queue" => Self::Queue,
            "cancel_previous" | "cancel-previous" => Self::CancelPrevious,
            _ => Self::Skip,
        }
    }
}

/// How a task remembers across runs. It has no chat history to accumulate, so
/// this is the only continuity it gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundContinuity {
    /// Every run starts clean.
    Fresh,
    /// Recent run summaries are injected as context. Required for anything
    /// touching people — a follow-up task that forgets yesterday contacts the
    /// same customer twice.
    Thread,
}

impl BackgroundContinuity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Thread => "thread",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "thread" => Self::Thread,
            _ => Self::Fresh,
        }
    }
}

/// Whether a task shows in the default UI list. Native core upkeep is
/// `Internal` so it doesn't bury the user's own tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundVisibility {
    Normal,
    Internal,
}

impl BackgroundVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Internal => "internal",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "internal" => Self::Internal,
            _ => Self::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundTaskStatus {
    Active,
    Paused,
    /// A one-shot trigger that has fired.
    Completed,
    /// Auto-quarantined after `max_failures` consecutive failures. Nobody is
    /// watching a background task, so it has to stop itself.
    Failed,
    Cancelled,
}

impl BackgroundTaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Active,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundRunStatus {
    Running,
    Success,
    Error,
    Timeout,
    Cancelled,
    /// Nothing to do (empty `template` context) or an overlap `Skip`. Counted
    /// apart from success in statistics: a skip is healthy, but a task that
    /// only ever skips is one to delete.
    Skipped,
}

impl BackgroundRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "running" => Self::Running,
            "error" => Self::Error,
            "timeout" => Self::Timeout,
            "cancelled" => Self::Cancelled,
            "skipped" => Self::Skipped,
            _ => Self::Success,
        }
    }
    /// True for statuses that count against `consecutive_failures`. A skip or
    /// a deliberate cancel is not the task's fault.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Error | Self::Timeout)
    }
}

/// What caused a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTriggerKind {
    Schedule,
    Manual,
    Install,
    CatchUp,
}

impl BackgroundTriggerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::Manual => "manual",
            Self::Install => "install",
            Self::CatchUp => "catch_up",
        }
    }
    pub fn parse(raw: &str) -> Self {
        match raw {
            "manual" => Self::Manual,
            "install" => Self::Install,
            "catch_up" | "catch-up" => Self::CatchUp,
            _ => Self::Schedule,
        }
    }
}

/// A registered unit of autonomous background work.
///
/// `(owner_id, owner_key)` is the idempotency key: re-installing an App upserts
/// its tasks rather than duplicating them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,

    pub owner_kind: BackgroundOwnerKind,
    pub owner_id: String,
    pub owner_key: String,
    pub title: String,
    pub description: Option<String>,

    pub job_kind: BackgroundJobKind,
    /// Native registry key when `job_kind = Native`.
    pub native_job: Option<String>,
    pub prompt_kind: BackgroundPromptKind,
    pub prompt: Option<String>,
    /// `prompt_kind = Template`: GET here for `{{var}}` values.
    pub context_url: Option<String>,

    pub persona: Option<String>,
    pub agent_folder: Option<String>,
    pub workspace_dir: Option<String>,
    /// Tool allowlist. Empty = the persona's own list, or all.
    pub use_tools: Vec<String>,
    /// JSON array of MCP server specs, injected per run.
    pub mcp_json: Option<String>,
    pub model_id: Option<String>,
    pub max_turns: Option<i64>,
    pub timeout_secs: Option<i64>,
    pub continuity: BackgroundContinuity,
    pub memory_folder: Option<String>,

    pub trigger_type: BackgroundTrigger,
    pub trigger_value: Option<String>,
    pub next_run: Option<String>,
    pub last_run: Option<String>,

    pub overlap_policy: OverlapPolicy,
    /// Run once for a window missed while the daemon was down.
    pub catch_up: bool,
    /// Consecutive failures before auto-pause. 0 = never.
    pub max_failures: i64,
    pub consecutive_failures: i64,
    pub visibility: BackgroundVisibility,
    /// Deliver an OS notification instead of running an agent. For "nhắc tôi /
    /// thông báo X" tasks: the runner pushes `title`/`prompt` straight to the
    /// desktop's notification system — fast, reliable, zero tokens — rather than
    /// spinning up an agent that has no notification tool and just flails.
    pub notify: bool,

    pub status: BackgroundTaskStatus,
    pub created_at: String,
    pub updated_at: String,
}

/// One execution of a [`BackgroundTask`] — i.e. one background session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundRun {
    pub id: String,
    pub task_id: String,
    /// `bg:<run_id>`. Passed as the engine's `instance_id`. Deliberately not a
    /// `GroupBinding` jid — no `groups` row is ever created for a background
    /// run, which is also why `is_dynamic_system_jid` needs no new prefix.
    pub session_id: String,
    pub trigger_kind: BackgroundTriggerKind,
    pub status: BackgroundRunStatus,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub turn_count: Option<i64>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    /// The prompt actually sent, after template/generator resolution.
    pub prompt: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// One line of a background session's transcript, fed by the runner's
/// `on_activity` stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundActivity {
    pub id: i64,
    pub run_id: String,
    pub ts: String,
    /// `think` | `text` | `tool` | `tool_error` | `message`
    pub kind: String,
    pub detail: Option<String>,
}

/// One shadow-git commit taken after a tool wrote into a chat's working
/// directory (see [`crate::checkpoints`]). `parent_sha` is `None` only for
/// the baseline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatCheckpoint {
    pub id: i64,
    pub chat_jid: String,
    pub sha: String,
    pub parent_sha: Option<String>,
    /// Tool that produced the change (`Edit`, `Write`, `Bash`, `restore`).
    pub tool_name: String,
    pub summary: String,
    /// Absolute working directory the commit was taken in.
    pub workspace: String,
    pub files_changed: i64,
    pub created_at: String,
}

//! Public types for the dispatch bridge.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ===== Public types =====

/// Subtask status — mirrors TS `DispatchTask.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DispatchTaskStatus {
    Registered,
    Processing,
    Done,
    Error,
    Timeout,
}

impl DispatchTaskStatus {
    /// Terminal statuses — DAG dependants may proceed once a task hits one of these.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Error | Self::Timeout)
    }
}

impl DispatchTaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Processing => "processing",
            Self::Done => "done",
            Self::Error => "error",
            Self::Timeout => "timeout",
        }
    }
}

/// How a task relates to the working directory it shares with its siblings.
///
/// Deserialization falls back to [`TaskIo::Exclusive`] for any unrecognised
/// value rather than failing: this field sits inside `dispatch-state.json`
/// alongside every parent, so a strict parse would make one bad string lose the
/// whole dispatch tree. Falling back to the *constrained* option keeps an
/// unknown value from silently granting parallelism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TaskIo {
    /// Writes nothing. Never blocks a peer and is never blocked — scouting,
    /// review, analysis and summarising all belong here.
    ReadOnly,
    /// Writes the paths in `DispatchTask::writes`. Blocks only against peers
    /// whose declaration could name the same file.
    #[default]
    Exclusive,
    /// Needs the working directory to itself. Until per-task worktrees exist
    /// this is honoured the only way it safely can be: as a whole-workspace
    /// lock, correct but not parallel.
    Isolated,
}

impl<'de> Deserialize<'de> for TaskIo {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(match raw.as_str() {
            "readOnly" | "read_only" => Self::ReadOnly,
            "isolated" => Self::Isolated,
            _ => Self::Exclusive,
        })
    }
}

/// Checklist item for task verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecklistItem {
    pub id: String,
    pub description: String,
    pub status: String, // "pending" | "completed" | "failed"
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub verification_note: Option<String>,
}

/// File change tracking for task verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: String,
    pub change_type: String, // "created" | "modified" | "deleted"
    #[serde(default)]
    pub lines_added: Option<i64>,
    #[serde(default)]
    pub lines_removed: Option<i64>,
    #[serde(default)]
    pub summary: Option<String>,
}

/// Verification result for a task or parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationResult {
    pub verified: bool,
    #[serde(default)]
    pub missing_items: Vec<String>,
    #[serde(default)]
    pub failed_items: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// One subtask inside a dispatch parent group.
/// Wire format mirrors TS `DispatchTask` so the Web Agent Console can render
/// agent names (incl. virtual/persona tasks) without a translation layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchTask {
    pub id: String,
    pub label: String,
    /// Persisted agents: folder. Virtual agents: `"persona:<personaName>"`.
    pub agent_id: String,
    /// Persisted agents: jid. Virtual agents: empty string.
    pub agent_jid: String,
    pub depends_on: Vec<String>,
    pub prompt: String,
    pub status: DispatchTaskStatus,
    pub result: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    /// Timeout budget supplied at creation (seconds); preserved across restarts.
    #[serde(default)]
    pub timeout_seconds: u64,
    pub timeout_at: Option<String>,
    pub completed_at: Option<String>,
    /// True when this task targets a virtual (persona-backed) worker.
    #[serde(default)]
    pub is_virtual: bool,
    /// Persona name when `is_virtual` is true.
    #[serde(default)]
    pub persona_name: Option<String>,
    /// Checklist items for task verification.
    #[serde(default)]
    pub checklist: Vec<ChecklistItem>,
    /// True when `checklist` was auto-generated from the prompt rather than
    /// supplied explicitly by the orchestrator. Auto checklists are advisory:
    /// a failed verification downgrades to warnings instead of `Error`.
    #[serde(default)]
    pub checklist_auto: bool,
    /// Number of scheduler-level retries already consumed (infra errors only).
    #[serde(default)]
    pub retry_count: u32,
    /// File changes tracked during task execution.
    #[serde(default)]
    pub file_changes: Vec<FileChange>,
    /// Verification result from checklist verification.
    #[serde(default)]
    pub verification_result: Option<VerificationResult>,
    /// How this task uses the shared working directory.
    #[serde(default)]
    pub io: TaskIo,
    /// Globs this task declares it will write. Empty means **undeclared**, not
    /// "writes nothing" — an undeclared task is left unconstrained so existing
    /// DAGs keep the parallelism they have today. A task that truly writes
    /// nothing should say `io: readOnly`.
    #[serde(default)]
    pub writes: Vec<String>,
    /// Where the task runs relative to the shared workspace.
    #[serde(default)]
    pub isolation: TaskIsolation,
    /// Filled when `isolation = worktree` and the workspace is a git
    /// repository: the checkout the task ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<crate::worktree::WorktreeInfo>,
}

/// How a task's writes relate to the shared workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TaskIsolation {
    /// Edits land in the shared working directory (write-set rules apply).
    #[default]
    None,
    /// Edits land in a git worktree on branch `senclaw/<parent>-<label>`;
    /// the shared checkout is untouched until a person merges. Requires the
    /// workspace to be a git repository — otherwise the task falls back to
    /// `none` and says so in its result.
    Worktree,
}

/// Parent dispatch (one `dispatch_task` MCP call → N subtasks).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchParent {
    pub id: String,
    pub goal: String,
    pub admin_folder: String,
    /// The chat that asked for this DAG.
    ///
    /// `admin_folder` is the *agent profile*, and every session started under
    /// one profile shares it — so a folder-only card filter shows a brand-new
    /// chat every DAG that profile has ever run. `None` on parents written
    /// before this field existed; the clients fall back to matching the
    /// creating tool call, which is already per-chat.
    #[serde(default)]
    pub chat_jid: Option<String>,
    /// Workspace path shared by child tasks under this parent.
    pub shared_workspace: Option<String>,
    /// "queued" / "active" / "done" — matches Web `DispatchParent.status`.
    pub status: String,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub tasks: Vec<DispatchTask>,
}

/// Persisted reference to a registered (persistent) agent. Mirrors the
/// `agents[]` array in the TS state file so external dispatch tooling
/// (CLI, MCP) can resolve `name → jid` without re-querying the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchAgent {
    pub name: String,
    /// Folder identifier (matches `GroupBinding.folder`).
    pub id: String,
    pub jid: String,
    pub channel: String,
}

/// Top-level state file shape (`~/.senclaw/dispatch-state.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DispatchState {
    /// Monotonic sequence used to generate `p-…` and `d-…` IDs.
    #[serde(rename = "_seq", default)]
    pub seq: u64,
    #[serde(default)]
    pub agents: Vec<DispatchAgent>,
    #[serde(default)]
    pub parents: Vec<DispatchParent>,
}

/// Callback fired when subtask activity (start/complete/error) should reset
/// the admin agent's inactivity timer.
pub type AdminActivityCallback = Arc<dyn Fn(&str) + Send + Sync>;

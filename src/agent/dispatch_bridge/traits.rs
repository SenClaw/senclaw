//! API trait and no-op stub for dispatch bridge.

use super::resume::build_dispatch_resume_hint;
use super::types::{AdminActivityCallback, DispatchParent};

// ===== API trait =====

/// Operations AgentPool calls on DispatchBridge.
///
/// Default no-op implementations let partial wiring compile; a concrete
/// `DispatchBridge` will replace them in a later phase.
#[allow(unused_variables)]
pub trait DispatchBridgeApi: Send + Sync {
    /// Notify that a dispatch task completed successfully (with optional final reply).
    fn notify_task_done(&self, task_id: &str, content: &str) {}

    /// Compatibility path when the taskId is unknown — match by agent JID.
    fn notify_reply(&self, agent_jid: &str, content: &str) {}

    /// Notify that the agent for a dispatch task errored / timed out.
    fn notify_error(&self, agent_jid: &str, error: &str) {}

    /// Snapshot of all parent dispatches (used by AgentPool to build resume hints).
    fn get_parents(&self) -> Vec<DispatchParent> {
        Vec::new()
    }

    /// Inject a callback fired on subtask activity, used by AgentPool to reset
    /// the admin agent's inactivity timer.
    fn set_admin_activity_callback(&self, cb: AdminActivityCallback) {}

    /// Whether there are active dispatch tasks under `folder`.
    fn has_active_dispatch(&self, _folder: &str) -> bool {
        false
    }

    /// Pause dispatch scheduling for `folder`; returns child JIDs to pause.
    fn pause_admin(&self, _folder: &str) -> Vec<String> {
        Vec::new()
    }

    /// Resume dispatch scheduling for `folder`.
    fn resume_admin(&self, _folder: &str) {}

    /// Cancel all active/queued parents for `folder`; returns child JIDs to stop.
    fn cancel_admin_parents(&self, _folder: &str) -> Vec<String> {
        Vec::new()
    }

    /// Set file changes for a dispatch task. Called by AgentPool during task execution.
    fn set_task_file_changes(&self, _task_id: &str, _file_changes: Vec<super::types::FileChange>) {}

    /// Add a single file change to a task. Useful for incremental tracking.
    fn add_file_change(&self, _task_id: &str, _path: &str, _change_type: &str) {}

    /// Build a resume hint for the admin agent listing active dispatches.
    fn build_dispatch_resume_hint(&self, folder: &str) -> String
    where
        Self: Sized,
    {
        build_dispatch_resume_hint(Some(self), folder).unwrap_or_default()
    }
}

/// No-op stub used until a real DispatchBridge ships.
pub struct NoopDispatchBridge;

impl DispatchBridgeApi for NoopDispatchBridge {}

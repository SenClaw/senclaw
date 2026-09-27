//! Dispatch bridge — DAG task orchestration for the main agent.
//! Port target: src-old/agent/DispatchBridge.ts (777 lines).
//!
//! Phases ported so far:
//! * Phase 1 — types, trait surface, JSON state file persistence with PID lock,
//!   parent/task CRUD via [`DispatchBridge::modify_state`], stale-state recovery
//!   on startup, agent list sync, pause/resume/cancel/has-active-dispatch,
//!   notify_task_done/notify_task_error (state mutation only — no scheduler yet).
//! * Phase 2 — WS notify callback fired on every state mutation; admin-activity
//!   heartbeat task that pings active-parent admins every 2 minutes so their
//!   inactivity timer doesn't fire mid-dispatch.
//!
//! Phase 3+ (DAG scheduler / timeout watcher / virtual workers) lands later.

pub mod bridge;
pub mod bridge_api;
pub mod dag;
pub mod file_tracker;
pub mod locks;
pub mod report;
pub mod resume;
pub mod traits;
pub mod types;
pub mod verification;
pub mod write_set;

#[cfg(test)]
mod tests;

// Re-export all public types for external users.
pub use bridge::{
    DispatchBridge, RevertWorkspaceCallback, SendToAgentCallback, TaskLifecycleCallback,
    WsNotifyCallback,
};
pub use dag::is_ready;
pub use file_tracker::{written_path_from_tool, FileTracker};
pub use report::{
    generate_actionable_next_steps, generate_parent_verification_report,
    generate_task_verification_report,
};
pub use resume::build_dispatch_resume_hint;
pub use traits::{DispatchBridgeApi, NoopDispatchBridge};
pub use types::{
    AdminActivityCallback, ChecklistItem, DispatchAgent, DispatchParent, DispatchState,
    DispatchTask, DispatchTaskStatus, FileChange, TaskIo, TaskIsolation, VerificationResult,
};
pub use verification::{
    generate_checklist_from_prompt, verify_parent_checklist, verify_task_checklist,
};

// pub(crate) items re-exported for intra-crate use.
pub(crate) use locks::{modify_state_file, read_state_file};

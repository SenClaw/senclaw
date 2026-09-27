//! Background tasks — autonomous work SenClaw runs by itself.
//!
//! A background run is **not a chat session**: no `GroupBinding`, no
//! `channel_messages`, no reply to anybody. It is one `run_one_shot` call whose
//! transcript lands in `background_activity` and whose outcome lands in
//! `background_runs`. See `docs/background-tasks-design.md`.
//!
//! Deliberately separate from [`crate::scheduler`], which is the *user's*
//! schedule and runs prompts inside a chat.
//!
//! ```text
//! BackgroundScheduler::tick   → due tasks, spawned concurrently
//!   └─ BackgroundRunner::execute
//!        ├─ resolve prompt (static | template+contextUrl | generator)
//!        ├─ run_one_shot(instance_id = "bg:<run_id>")
//!        └─ record run + activity, backoff on failure
//! ```

mod native;
mod runner;
mod scheduler;

pub use native::{native_job, NativeJobFn, NativeRegistry};
pub use runner::BackgroundRunner;
pub use scheduler::{plan_next_run, BackgroundScheduler};

use crate::types::{BackgroundRunStatus, BackgroundTask, BackgroundTriggerKind};

/// UI notification seam for background runs.
///
/// Every method defaults to a no-op, mirroring [`crate::agent::agent_pool::traits::AgentEventSink`]
/// — an implementor overrides only what it cares about, and the daemon can run
/// headless without one.
///
/// This fills a real gap: the existing scheduler pushes **nothing** when a task
/// fires, succeeds, or fails; its WS messages are strictly request/response.
pub trait BackgroundEventSink: Send + Sync {
    fn run_started(&self, _task: &BackgroundTask, _run_id: &str, _trigger: BackgroundTriggerKind) {}
    fn run_activity(&self, _task_id: &str, _run_id: &str, _kind: &str, _detail: &str) {}
    fn run_finished(
        &self,
        _task_id: &str,
        _run_id: &str,
        _status: BackgroundRunStatus,
        _duration_ms: i64,
        _error: Option<&str>,
    ) {
    }
    fn task_changed(&self, _task: &BackgroundTask) {}
    /// Push an OS/desktop notification (title + message). Used by notify-only
    /// tasks; distinct from a chat reply — it's a fire-and-forget push, which is
    /// the one way a background task legitimately reaches the user.
    fn notify(&self, _title: &str, _message: &str) {}
}

/// Drop-in sink for tests and headless runs.
pub struct NoopBackgroundEventSink;
impl BackgroundEventSink for NoopBackgroundEventSink {}

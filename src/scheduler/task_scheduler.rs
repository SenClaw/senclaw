//! Polling task scheduler. Mirrors `src-old/scheduler/TaskScheduler.ts`.
//!
//! Every `interval_sec` seconds the scheduler scans the DB for active tasks
//! whose `next_run <= now`, advances each one's next_run, and hands the task
//! to a [`TaskExecutor`]. The executor encapsulates the actual `context_mode`
//! handling (notify / isolated / group / script / script-agent) — all of
//! which depend on agent + channel modules that have not been ported yet.
//! The scheduler itself is fully testable with a mock executor.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cron::Schedule;
use tokio::task::JoinHandle;

use crate::db::Db;
use crate::types::{ScheduleType, ScheduledTask, TaskStatus};

/// Strategy seam for actually running a task. Concrete impl will live in the
/// `agent` layer once it exists.
#[async_trait]
pub trait TaskExecutor: Send + Sync + 'static {
    /// Called for every due task. The scheduler has already advanced
    /// `next_run` and `status` in the DB; the executor is responsible for the
    /// `context_mode` handling and writing the run-log + last_result.
    async fn execute(&self, task: ScheduledTask);
}

pub struct TaskScheduler {
    db: Arc<Db>,
    executor: Arc<dyn TaskExecutor>,
    interval: Duration,
}

impl TaskScheduler {
    pub fn new(db: Arc<Db>, executor: Arc<dyn TaskExecutor>, interval_sec: u64) -> Self {
        Self {
            db,
            executor,
            interval: Duration::from_secs(interval_sec.max(1)),
        }
    }

    /// Spawn the polling loop on the current Tokio runtime. Drop the returned
    /// handle (or call `abort()`) to stop.
    pub fn start(self) -> JoinHandle<()> {
        let interval = self.interval;
        let tz_name = chrono::Local::now().format("%Z %z").to_string();
        tracing::info!(
            interval_sec = interval.as_secs(),
            local_tz = %tz_name,
            "[TaskScheduler] Started (cron expressions evaluated in local timezone)"
        );
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                if let Err(e) = self.tick().await {
                    tracing::error!(error = %e, "[TaskScheduler] tick failed");
                }
            }
        })
    }

    /// Single poll cycle. Public so tests can drive it deterministically.
    pub async fn tick(&self) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let due = self.db.get_due_tasks(&now)?;
        if !due.is_empty() {
            tracing::info!(count = due.len(), "[TaskScheduler] due task(s) found");
        }
        for task in due {
            self.dispatch(task).await;
        }
        Ok(())
    }

    /// Advance the task's next_run + status, then hand off to the executor.
    /// We advance *before* execution so a slow handler can't cause re-pickup
    /// on the next tick.
    async fn dispatch(&self, task: ScheduledTask) {
        // `once_delete`: remove the row up-front (same re-pickup guard as the
        // advance below) and then run it. The run still records to
        // `task_run_logs`, but the schedule itself is gone afterward.
        if task.schedule_type == ScheduleType::OnceDelete {
            if let Err(e) = self.db.delete_task(&task.id) {
                tracing::error!(task_id = %task.id, error = %e, "[TaskScheduler] delete (once_delete) failed");
                return;
            }
            tracing::info!(task_id = %task.id, "[TaskScheduler] once_delete task fired and removed");
            self.executor.execute(task).await;
            return;
        }

        let next_run = compute_next_run(&task);
        let next_status = if task.schedule_type == ScheduleType::Once {
            TaskStatus::Completed
        } else {
            TaskStatus::Active
        };
        if let Err(e) = self
            .db
            .advance_task_next_run(&task.id, next_run.as_deref(), next_status)
        {
            tracing::error!(task_id = %task.id, error = %e, "[TaskScheduler] advance failed");
            return;
        }
        self.executor.execute(task).await;
    }
}

/// Compute the next run timestamp for a task. Returns `None` for one-shot
/// tasks or when the schedule value can't be parsed.
///
/// * `interval` schedules: `schedule_value` is milliseconds (kept for parity
///   with TS). Base = previous `next_run` if present, else now.
/// * `cron` schedules: parsed via the `cron` crate. Note this crate uses the
///   6-field form (`sec min hour dom mon dow`) — it differs from `cron-parser`
///   in TS which accepts the 5-field form. See [`compute_next_run`] for how we
///   normalize 5-field input by prefixing `0 ` (run at the top of the second).
pub fn compute_next_run(task: &ScheduledTask) -> Option<String> {
    match task.schedule_type {
        // One-shot types never produce a subsequent run.
        ScheduleType::Once | ScheduleType::OnceDelete => None,
        ScheduleType::Interval => {
            let ms: i64 = task.schedule_value.parse().ok()?;
            if ms <= 0 {
                return None;
            }
            let base = task
                .next_run
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            let next = base + chrono::Duration::milliseconds(ms);
            Some(next.to_rfc3339())
        }
        ScheduleType::Cron => {
            let normalized = normalize_cron_expr(&task.schedule_value);
            let schedule = Schedule::from_str(&normalized).ok()?;
            // Use the host's local timezone so cron expressions like "0 9 * * *"
            // fire at 09:00 local time, not 09:00 UTC.
            let local_next = schedule.upcoming(chrono::Local).next()?;
            Some(local_next.with_timezone(&Utc).to_rfc3339())
        }
    }
}

/// Accept both the standard 5-field cron expression (used by `cron-parser` in
/// the TS code) and the 6-field form required by the Rust `cron` crate. The
/// extra leading field is "seconds"; we prepend "0 " so 5-field expressions
/// keep firing exactly as before.
fn normalize_cron_expr(expr: &str) -> String {
    let trimmed = expr.trim();
    let field_count = trimmed.split_whitespace().count();
    if field_count == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentMode, ContextMode, ScheduleType};
    use std::sync::Mutex;

    fn task(schedule_type: ScheduleType, value: &str, next_run: Option<&str>) -> ScheduledTask {
        ScheduledTask {
            id: "t1".into(),
            group_folder: "f".into(),
            chat_jid: "tg:group:1".into(),
            prompt: "p".into(),
            schedule_type,
            schedule_value: value.into(),
            context_mode: ContextMode::Isolated,
            agent_mode: AgentMode::Agent,
            script_command: None,
            watch_json: None,
            next_run: next_run.map(String::from),
            last_run: None,
            last_result: None,
            status: TaskStatus::Active,
            created_at: "2026-04-28T00:00:00Z".into(),
        }
    }

    #[test]
    fn next_run_for_once_is_none() {
        assert!(compute_next_run(&task(ScheduleType::Once, "ignored", None)).is_none());
    }

    #[test]
    fn next_run_for_once_delete_is_none() {
        assert!(compute_next_run(&task(ScheduleType::OnceDelete, "ignored", None)).is_none());
    }

    #[test]
    fn next_run_for_interval_uses_previous_next_run() {
        // 60_000 ms past the previous next_run.
        let t = task(
            ScheduleType::Interval,
            "60000",
            Some("2026-04-28T00:00:00Z"),
        );
        let next = compute_next_run(&t).unwrap();
        let parsed = DateTime::parse_from_rfc3339(&next).unwrap();
        let prev = DateTime::parse_from_rfc3339("2026-04-28T00:00:00Z").unwrap();
        assert_eq!(parsed.timestamp() - prev.timestamp(), 60);
    }

    #[test]
    fn next_run_for_interval_with_no_prev_uses_now() {
        let t = task(ScheduleType::Interval, "1000", None);
        let next = compute_next_run(&t).unwrap();
        // Just confirm we got a valid RFC3339 timestamp roughly ≈ now.
        let parsed = DateTime::parse_from_rfc3339(&next).unwrap();
        let delta = (parsed.timestamp() - Utc::now().timestamp()).abs();
        assert!(delta < 5, "next_run should be close to now");
    }

    #[test]
    fn next_run_invalid_interval_is_none() {
        assert!(compute_next_run(&task(ScheduleType::Interval, "abc", None)).is_none());
        assert!(compute_next_run(&task(ScheduleType::Interval, "0", None)).is_none());
        assert!(compute_next_run(&task(ScheduleType::Interval, "-5", None)).is_none());
    }

    #[test]
    fn next_run_cron_5_field_normalised() {
        // every minute
        let t = task(ScheduleType::Cron, "* * * * *", None);
        let next = compute_next_run(&t).expect("should produce next run");
        // Just sanity-check it parses.
        DateTime::parse_from_rfc3339(&next).unwrap();
    }

    #[test]
    fn next_run_cron_6_field_passthrough() {
        let t = task(ScheduleType::Cron, "0 0 * * * *", None);
        assert!(compute_next_run(&t).is_some());
    }

    #[test]
    fn next_run_cron_invalid_returns_none() {
        let t = task(ScheduleType::Cron, "not-a-cron", None);
        assert!(compute_next_run(&t).is_none());
    }

    // ─── tick / dispatch integration ────────────────────────────────

    struct Recorder(Mutex<Vec<String>>);
    #[async_trait]
    impl TaskExecutor for Recorder {
        async fn execute(&self, task: ScheduledTask) {
            self.0.lock().unwrap().push(task.id);
        }
    }

    #[tokio::test]
    async fn tick_dispatches_due_tasks_and_advances() {
        let cfg = crate::config::Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        // Insert a due task.
        let mut t = task(
            ScheduleType::Interval,
            "60000",
            Some("2020-01-01T00:00:00Z"),
        );
        t.id = "due".into();
        db.insert_task(&t).unwrap();
        // Insert a far-future task.
        let mut future = t.clone();
        future.id = "future".into();
        future.next_run = Some("2999-01-01T00:00:00Z".into());
        db.insert_task(&future).unwrap();

        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        let scheduler = TaskScheduler::new(db.clone(), recorder.clone(), 60);
        scheduler.tick().await.unwrap();

        let dispatched = recorder.0.lock().unwrap().clone();
        assert_eq!(dispatched, vec!["due"]);

        // next_run should have advanced for the due task.
        let after = db.list_all_tasks().unwrap();
        let due = after.iter().find(|x| x.id == "due").unwrap();
        let advanced = DateTime::parse_from_rfc3339(due.next_run.as_deref().unwrap()).unwrap();
        let original = DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z").unwrap();
        assert!(advanced > original);
    }

    #[tokio::test]
    async fn once_task_marked_completed_after_dispatch() {
        let cfg = crate::config::Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let mut t = task(ScheduleType::Once, "ignored", Some("2020-01-01T00:00:00Z"));
        t.schedule_type = ScheduleType::Once;
        db.insert_task(&t).unwrap();

        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        TaskScheduler::new(db.clone(), recorder, 60)
            .tick()
            .await
            .unwrap();

        let after = db.list_all_tasks().unwrap();
        assert_eq!(after[0].status, TaskStatus::Completed);
        assert!(after[0].next_run.is_none());
    }

    #[tokio::test]
    async fn once_delete_task_removed_after_dispatch() {
        let cfg = crate::config::Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let mut t = task(
            ScheduleType::OnceDelete,
            "ignored",
            Some("2020-01-01T00:00:00Z"),
        );
        t.id = "one-shot".into();
        db.insert_task(&t).unwrap();

        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        TaskScheduler::new(db.clone(), recorder.clone(), 60)
            .tick()
            .await
            .unwrap();

        // It ran exactly once...
        assert_eq!(recorder.0.lock().unwrap().clone(), vec!["one-shot"]);
        // ...and the row is gone entirely (not just marked completed).
        assert!(db.list_all_tasks().unwrap().is_empty());
    }
}

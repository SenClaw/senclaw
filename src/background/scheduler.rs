//! The background poll loop.
//!
//! Modelled on [`crate::agent::mcp_dispatch::MCPDispatcher::tick`] rather than
//! on [`crate::scheduler::TaskScheduler::tick`], because the latter has three
//! properties a background scheduler must not inherit:
//!
//! * it `await`s each due task in a `for` loop, so one slow run blocks every
//!   other due task behind it — and background runs are long by nature;
//! * it has no overlap policy, so a 5-minute task on a 1-minute interval piles
//!   up without bound;
//! * a task that fails every single run stays `active` forever.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use cron::Schedule;
use std::str::FromStr;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{BackgroundEventSink, BackgroundRunner, NativeRegistry, NoopBackgroundEventSink};
use crate::agent::persona_registry::PersonaRegistry;
use crate::config::BackgroundConfig;
use crate::db::Db;
use crate::types::{
    BackgroundRun, BackgroundRunStatus, BackgroundTask, BackgroundTaskStatus, BackgroundTrigger,
    BackgroundTriggerKind,
};

/// Floor on the poll cadence, so a misconfigured interval can't spin the loop.
const MIN_INTERVAL_SECS: u64 = 5;
/// How late a window must be before we treat it as "the daemon was down"
/// rather than ordinary tick jitter. Below this, a due task always runs.
const DOWNTIME_THRESHOLD_SECS: i64 = 300;

struct InFlight {
    run_id: String,
    cancel: CancellationToken,
    owner_id: String,
}

pub struct BackgroundScheduler {
    db: Arc<Db>,
    cfg: BackgroundConfig,
    runner: Arc<BackgroundRunner>,
    sem: Arc<Semaphore>,
    /// task_id → in-flight run. Drives the overlap policy.
    in_flight: std::sync::Mutex<HashMap<String, InFlight>>,
    events: Arc<dyn BackgroundEventSink>,
}

impl BackgroundScheduler {
    pub fn new(
        db: Arc<Db>,
        cfg: BackgroundConfig,
        personas: Option<Arc<std::sync::Mutex<PersonaRegistry>>>,
        native: Arc<NativeRegistry>,
        events: Option<Arc<dyn BackgroundEventSink>>,
        scratch_dir: String,
    ) -> Arc<Self> {
        let events: Arc<dyn BackgroundEventSink> =
            events.unwrap_or_else(|| Arc::new(NoopBackgroundEventSink));
        let runner = Arc::new(BackgroundRunner {
            db: db.clone(),
            cfg: cfg.clone(),
            personas,
            events: events.clone(),
            native,
            scratch_dir,
        });
        Arc::new(Self {
            db,
            sem: Arc::new(Semaphore::new(cfg.max_concurrent.max(1))),
            cfg,
            runner,
            in_flight: std::sync::Mutex::new(HashMap::new()),
            events,
        })
    }

    /// Spawn the poll loop. **Hold the returned handle** — `_task_scheduler` and
    /// `_event_notifier` in `run_daemon` are bound to `_` locals and dropped, so
    /// they have no abort path on shutdown. Don't repeat that.
    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {
        let this = self.clone();
        let interval = Duration::from_secs(this.cfg.interval_secs.max(MIN_INTERVAL_SECS));

        // A run marked `running` at boot belonged to a process that no longer
        // exists; nothing will ever finish it.
        match this.db.reclaim_orphan_background_runs() {
            Ok(n) if n > 0 => {
                tracing::warn!("[background] reclaimed {n} orphan run(s) from a previous process")
            }
            Err(e) => tracing::error!(error = %e, "[background] orphan reclaim failed"),
            _ => {}
        }

        tracing::info!(
            interval_secs = interval.as_secs(),
            max_concurrent = this.cfg.max_concurrent,
            enabled = this.cfg.enabled,
            "[background] scheduler started"
        );

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut ticks: u64 = 0;
            loop {
                ticker.tick().await;
                if let Err(e) = this.tick().await {
                    tracing::error!(error = %e, "[background] tick failed");
                }
                // Retention, roughly hourly. Cheap enough to keep in the loop.
                ticks += 1;
                if ticks % (3600 / interval.as_secs().max(1)).max(1) == 0 {
                    match this.db.prune_background_runs(this.cfg.retention_days) {
                        Ok(n) if n > 0 => tracing::info!("[background] pruned {n} old run(s)"),
                        Err(e) => tracing::warn!(error = %e, "[background] prune failed"),
                        _ => {}
                    }
                }
            }
        })
    }

    /// One poll cycle. Public so tests can drive it deterministically.
    pub async fn tick(self: &Arc<Self>) -> Result<()> {
        if !self.cfg.enabled {
            return Ok(());
        }
        let now = Utc::now();
        let due = self.db.get_due_background_tasks(&now.to_rfc3339())?;
        for task in due {
            self.dispatch(task, now).await;
        }
        Ok(())
    }

    /// Decide whether/how a due task fires, then spawn it. Never awaits the run
    /// itself — that is the whole point.
    async fn dispatch(self: &Arc<Self>, task: BackgroundTask, now: DateTime<Utc>) {
        let next_run = plan_next_run(&task, now);

        // Was this window missed while the daemon was down?
        let lateness = task
            .next_run
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| (now - t.with_timezone(&Utc)).num_seconds())
            .unwrap_or(0);
        if lateness > DOWNTIME_THRESHOLD_SECS && !task.catch_up {
            // Record the skip rather than silently swallowing it: a task that
            // quietly didn't run looks identical to one that ran and found
            // nothing, and the user is judging these from the run history.
            self.record_synthetic_run(
                &task,
                BackgroundRunStatus::Skipped,
                &format!("missed window ({lateness}s late, daemon likely down); catch_up is off"),
            );
            self.advance(&task, next_run.as_deref());
            return;
        }

        match self.overlap_check(&task) {
            Overlap::Free => {}
            Overlap::Skip => {
                self.record_synthetic_run(
                    &task,
                    BackgroundRunStatus::Skipped,
                    "previous run still in flight (overlap policy: skip)",
                );
                self.advance(&task, next_run.as_deref());
                return;
            }
            Overlap::Wait => {
                // Leave next_run alone: the next tick retries, which is
                // "wait for the previous run" at tick granularity, without
                // needing to hold a second task in memory.
                tracing::debug!(
                    task_id = %task.id,
                    "[background] previous run in flight; queued for next tick"
                );
                return;
            }
            Overlap::CancelledPrevious => {
                tracing::info!(
                    task_id = %task.id,
                    "[background] cancelled previous run (overlap policy: cancel_previous)"
                );
            }
        }

        // Per-owner cap, so one App can't starve everything else.
        if self.active_for_owner(&task.owner_id) >= self.cfg.per_owner.max(1) {
            tracing::debug!(
                task_id = %task.id, owner = %task.owner_id,
                "[background] owner at concurrency cap; retrying next tick"
            );
            return;
        }

        self.advance(&task, next_run.as_deref());

        let trigger = if lateness > DOWNTIME_THRESHOLD_SECS {
            BackgroundTriggerKind::CatchUp
        } else {
            BackgroundTriggerKind::Schedule
        };
        self.spawn_run(task, trigger);
    }

    /// Run a task right now, outside the schedule.
    ///
    /// Executes **inline** and returns the run id. The existing
    /// `recurring_run_now` instead rewinds `next_run` to `now - 1s` and lets the
    /// 30 s poll pick it up, which makes the UI's run-now button look dead for
    /// half a minute.
    pub async fn run_now(self: &Arc<Self>, task_id: &str) -> Result<String> {
        let task = self
            .db
            .get_background_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("no such background task: {task_id}"))?;

        if let Overlap::Skip | Overlap::Wait = self.overlap_check(&task) {
            anyhow::bail!("a run of this task is already in flight");
        }
        let _permit = self.sem.clone().acquire_owned().await?;
        let cancel = CancellationToken::new();
        self.track(&task, "pending", cancel.clone());
        let out = self
            .runner
            .execute(&task, BackgroundTriggerKind::Manual, cancel)
            .await;
        self.untrack(&task.id);
        Ok(out?.run_id)
    }

    /// Fire an App's `on_install` task once, at install time.
    pub fn run_on_install(self: &Arc<Self>, task: BackgroundTask) {
        self.spawn_run(task, BackgroundTriggerKind::Install);
    }

    pub async fn cancel_run(&self, run_id: &str) -> bool {
        let guard = self.in_flight.lock().unwrap();
        for f in guard.values() {
            if f.run_id == run_id {
                f.cancel.cancel();
                return true;
            }
        }
        false
    }

    pub fn cancel_task_runs(&self, task_id: &str) -> bool {
        let guard = self.in_flight.lock().unwrap();
        match guard.get(task_id) {
            Some(f) => {
                f.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Recompute `next_run` for a task the caller just edited or resumed.
    pub fn rearm(&self, task: &BackgroundTask) -> Option<String> {
        plan_next_run(task, Utc::now())
    }

    /// Push a task change to the UI.
    ///
    /// Exposed so the REST layer can notify without holding the WebSocket
    /// gateway itself — `UiState` has no gateway handle, and the event sink is
    /// meant to be the single path from background work to the UI.
    pub fn notify_task_changed(&self, task: &BackgroundTask) {
        self.events.task_changed(task);
    }

    // ─── internals ───────────────────────────────────────────────────────

    fn spawn_run(self: &Arc<Self>, task: BackgroundTask, trigger: BackgroundTriggerKind) {
        let this = self.clone();
        let cancel = CancellationToken::new();
        self.track(&task, "pending", cancel.clone());
        tokio::spawn(async move {
            // Bound total concurrency. Acquired inside the spawned task so the
            // tick loop never blocks on a busy pool.
            let _permit = match this.sem.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    this.untrack(&task.id);
                    return;
                }
            };
            let outcome = this.runner.execute(&task, trigger, cancel).await;
            this.untrack(&task.id);

            match outcome {
                Ok(o) if o.status.is_failure() => this.apply_backoff(&task),
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(task_id = %task.id, error = %e, "[background] run bookkeeping failed");
                }
            }
        });
    }

    /// Slow a failing task down: `next_run = max(scheduled, now + backoff)`.
    ///
    /// The `max` matters. A 1-minute task that fails should back off to minutes;
    /// a daily cron that fails should still be tomorrow, not tomorrow-plus-two-
    /// minutes — so whichever is later wins.
    fn apply_backoff(&self, task: &BackgroundTask) {
        let Ok(Some(fresh)) = self.db.get_background_task(&task.id) else {
            return;
        };
        if fresh.status != BackgroundTaskStatus::Active {
            return; // already quarantined
        }
        let n = fresh.consecutive_failures.clamp(1, 16) as u32;
        let backoff = (60_i64.saturating_mul(1_i64 << (n - 1))).min(self.cfg.backoff_max_secs);
        let floor = Utc::now() + chrono::Duration::seconds(backoff);
        let scheduled = fresh
            .next_run
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        let next = match scheduled {
            Some(s) if s > floor => s,
            _ => floor,
        };
        if let Err(e) =
            self.db
                .advance_background_next_run(&task.id, Some(&next.to_rfc3339()), fresh.status)
        {
            tracing::warn!(task_id = %task.id, error = %e, "[background] backoff write failed");
        } else {
            tracing::info!(
                task_id = %task.id, failures = n, backoff_secs = backoff,
                next_run = %next.to_rfc3339(), "[background] backing off after failure"
            );
        }
    }

    fn overlap_check(&self, task: &BackgroundTask) -> Overlap {
        let guard = self.in_flight.lock().unwrap();
        let Some(f) = guard.get(&task.id) else {
            return Overlap::Free;
        };
        match task.overlap_policy {
            crate::types::OverlapPolicy::Skip => Overlap::Skip,
            crate::types::OverlapPolicy::Queue => Overlap::Wait,
            crate::types::OverlapPolicy::CancelPrevious => {
                f.cancel.cancel();
                Overlap::CancelledPrevious
            }
        }
    }

    fn active_for_owner(&self, owner_id: &str) -> usize {
        self.in_flight
            .lock()
            .unwrap()
            .values()
            .filter(|f| f.owner_id == owner_id)
            .count()
    }

    fn track(&self, task: &BackgroundTask, run_id: &str, cancel: CancellationToken) {
        self.in_flight.lock().unwrap().insert(
            task.id.clone(),
            InFlight {
                run_id: run_id.to_owned(),
                cancel,
                owner_id: task.owner_id.clone(),
            },
        );
    }

    fn untrack(&self, task_id: &str) {
        self.in_flight.lock().unwrap().remove(task_id);
    }

    fn advance(&self, task: &BackgroundTask, next_run: Option<&str>) {
        // A one-shot trigger with no next run is done, not perpetually active.
        let status = if next_run.is_none() && task.trigger_type.is_one_shot() {
            BackgroundTaskStatus::Completed
        } else {
            task.status
        };
        if let Err(e) = self
            .db
            .advance_background_next_run(&task.id, next_run, status)
        {
            tracing::error!(task_id = %task.id, error = %e, "[background] advance failed");
        }
    }

    /// Record a run that never invoked an agent (a skip). Keeps the history
    /// honest: "didn't run, here's why" instead of a silent gap.
    fn record_synthetic_run(&self, task: &BackgroundTask, status: BackgroundRunStatus, why: &str) {
        let run_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let run = BackgroundRun {
            id: run_id.clone(),
            task_id: task.id.clone(),
            session_id: format!("bg:{run_id}"),
            trigger_kind: BackgroundTriggerKind::Schedule,
            status,
            started_at: now.clone(),
            finished_at: Some(now),
            duration_ms: Some(0),
            turn_count: None,
            tokens_in: None,
            tokens_out: None,
            prompt: None,
            result: Some(why.to_owned()),
            error: None,
        };
        if let Err(e) = self.db.insert_background_run(&run) {
            tracing::warn!(task_id = %task.id, error = %e, "[background] synthetic run insert failed");
        }
        self.events.run_finished(&task.id, &run_id, status, 0, None);
    }
}

enum Overlap {
    Free,
    Skip,
    Wait,
    CancelledPrevious,
}

/// The next firing time after the one being handled now.
///
/// `None` for one-shot triggers — they never fire again.
pub fn plan_next_run(task: &BackgroundTask, now: DateTime<Utc>) -> Option<String> {
    match task.trigger_type {
        BackgroundTrigger::Once | BackgroundTrigger::OnInstall | BackgroundTrigger::Manual => None,

        BackgroundTrigger::Interval => {
            let ms: i64 = task.trigger_value.as_deref()?.parse().ok()?;
            if ms <= 0 {
                return None;
            }
            let base = task
                .next_run
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or(now);
            // Walk past every window missed while we were down, so a task that
            // was asleep for a week doesn't fire a week's worth of backlog on
            // the next few ticks. Bounded so a 1 ms interval can't spin here.
            let mut next = base + chrono::Duration::milliseconds(ms);
            let mut guard = 0;
            while next <= now && guard < 100_000 {
                next += chrono::Duration::milliseconds(ms);
                guard += 1;
            }
            Some(next.to_rfc3339())
        }

        BackgroundTrigger::Cron => {
            let expr = normalize_cron_expr(task.trigger_value.as_deref()?);
            let schedule = Schedule::from_str(&expr).ok()?;
            // Local timezone, so "0 9 * * *" means 09:00 where the user is —
            // same convention as the user-facing scheduler.
            let next = schedule.upcoming(chrono::Local).next()?;
            Some(next.with_timezone(&Utc).to_rfc3339())
        }
    }
}

/// Accept both the 5-field cron form people write and the 6-field form the
/// `cron` crate needs; the extra leading field is seconds.
fn normalize_cron_expr(expr: &str) -> String {
    let trimmed = expr.trim();
    if trimmed.split_whitespace().count() == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        BackgroundContinuity, BackgroundJobKind, BackgroundOwnerKind, BackgroundPromptKind,
        BackgroundVisibility, OverlapPolicy,
    };

    fn task(trigger: BackgroundTrigger, value: &str, next_run: Option<&str>) -> BackgroundTask {
        BackgroundTask {
            id: "t1".into(),
            owner_kind: BackgroundOwnerKind::User,
            owner_id: "main".into(),
            owner_key: "k".into(),
            title: "t".into(),
            description: None,
            job_kind: BackgroundJobKind::Prompt,
            native_job: None,
            prompt_kind: BackgroundPromptKind::Static,
            prompt: Some("p".into()),
            context_url: None,
            persona: None,
            agent_folder: None,
            workspace_dir: None,
            use_tools: Vec::new(),
            mcp_json: None,
            model_id: None,
            max_turns: None,
            timeout_secs: None,
            continuity: BackgroundContinuity::Fresh,
            memory_folder: None,
            trigger_type: trigger,
            trigger_value: Some(value.into()),
            next_run: next_run.map(str::to_owned),
            last_run: None,
            overlap_policy: OverlapPolicy::Skip,
            catch_up: false,
            max_failures: 5,
            consecutive_failures: 0,
            visibility: BackgroundVisibility::Normal,
            notify: false,
            status: BackgroundTaskStatus::Active,
            created_at: "2026-07-17T00:00:00Z".into(),
            updated_at: "2026-07-17T00:00:00Z".into(),
        }
    }

    #[test]
    fn one_shot_triggers_never_reschedule() {
        let now = Utc::now();
        for t in [
            BackgroundTrigger::Once,
            BackgroundTrigger::OnInstall,
            BackgroundTrigger::Manual,
        ] {
            assert!(
                plan_next_run(&task(t, "2026-07-17T09:00:00Z", None), now).is_none(),
                "{t:?} should not reschedule"
            );
        }
    }

    #[test]
    fn interval_advances_one_window_when_on_time() {
        let now = Utc::now();
        let t = task(
            BackgroundTrigger::Interval,
            "60000",
            Some(&now.to_rfc3339()),
        );
        let next = plan_next_run(&t, now).unwrap();
        let next = DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&Utc);
        let delta = (next - now).num_seconds();
        assert_eq!(delta, 60, "expected exactly one 60s window, got {delta}s");
    }

    #[test]
    fn interval_walks_past_a_long_outage_instead_of_queueing_a_backlog() {
        // Daemon down for an hour on a 5-minute task: the next run is one
        // window ahead, not 12 windows of catch-up.
        let now = Utc::now();
        let stale = now - chrono::Duration::hours(1);
        let t = task(
            BackgroundTrigger::Interval,
            "300000",
            Some(&stale.to_rfc3339()),
        );
        let next = plan_next_run(&t, now).unwrap();
        let next = DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&Utc);
        assert!(next > now, "next run must be in the future");
        assert!(
            (next - now).num_seconds() <= 300,
            "next run should be within one window, got {}s",
            (next - now).num_seconds()
        );
    }

    #[test]
    fn interval_rejects_nonsense_values() {
        let now = Utc::now();
        assert!(plan_next_run(&task(BackgroundTrigger::Interval, "0", None), now).is_none());
        assert!(plan_next_run(&task(BackgroundTrigger::Interval, "-5", None), now).is_none());
        assert!(plan_next_run(&task(BackgroundTrigger::Interval, "abc", None), now).is_none());
    }

    #[test]
    fn cron_accepts_the_five_field_form_users_write() {
        let now = Utc::now();
        let next = plan_next_run(&task(BackgroundTrigger::Cron, "0 9 * * *", None), now);
        assert!(next.is_some(), "5-field cron must be accepted");
        let next = DateTime::parse_from_rfc3339(&next.unwrap()).unwrap();
        assert!(next.with_timezone(&Utc) > now);
    }

    #[test]
    fn cron_normalization_only_touches_the_five_field_form() {
        assert_eq!(normalize_cron_expr("0 9 * * *"), "0 0 9 * * *");
        assert_eq!(normalize_cron_expr("30 0 9 * * *"), "30 0 9 * * *");
        assert_eq!(normalize_cron_expr("  0 9 * * *  "), "0 0 9 * * *");
    }

    #[test]
    fn cron_rejects_garbage_rather_than_firing_wrongly() {
        let now = Utc::now();
        assert!(plan_next_run(&task(BackgroundTrigger::Cron, "not a cron", None), now).is_none());
    }
}

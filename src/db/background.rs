//! Background task accessors. See `docs/background-tasks-design.md`.
//!
//! Deliberately separate from [`super::scheduled_tasks`]: that table is the
//! user's schedule (runs in a chat, replies to a human), this one is autonomous
//! work owned by core or by a Space App.
//!
//! Two departures from the scheduled-task accessors, both intentional:
//!
//! * `last_run` is written on **every** run. In `scheduled_tasks` the only
//!   writer of that column (`update_task_run`) is called exclusively from
//!   tests, so the column is permanently NULL and every consumer of it renders
//!   empty. Don't repeat that.
//! * Failures are counted. Nobody is watching a background task, so it has to
//!   quarantine itself — see [`Db::record_background_failure`].

use anyhow::Result;
use rusqlite::{params, params_from_iter, Row};
use serde::Serialize;

use crate::types::{
    BackgroundActivity, BackgroundContinuity, BackgroundJobKind, BackgroundOwnerKind,
    BackgroundPromptKind, BackgroundRun, BackgroundRunStatus, BackgroundTask, BackgroundTaskStatus,
    BackgroundTrigger, BackgroundTriggerKind, BackgroundVisibility, OverlapPolicy,
};

/// Filter for [`Db::list_background_tasks`]. All-`None` + `include_internal:
/// false` is the default UI list.
#[derive(Debug, Clone, Default)]
pub struct BackgroundTaskFilter {
    pub owner_kind: Option<String>,
    pub owner_id: Option<String>,
    pub status: Option<String>,
    /// Include `visibility = 'internal'` tasks (native core upkeep).
    pub include_internal: bool,
    /// Page size. `None` = no limit (return all matching).
    pub limit: Option<i64>,
    /// Rows to skip for pagination.
    pub offset: Option<i64>,
}

fn row_to_background_task(row: &Row<'_>) -> rusqlite::Result<BackgroundTask> {
    let use_tools: Vec<String> = row
        .get::<_, Option<String>>("use_tools")?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    Ok(BackgroundTask {
        id: row.get("id")?,
        owner_kind: BackgroundOwnerKind::parse(&row.get::<_, String>("owner_kind")?),
        owner_id: row.get("owner_id")?,
        owner_key: row.get("owner_key")?,
        title: row.get("title")?,
        description: row.get("description")?,
        job_kind: BackgroundJobKind::parse(&row.get::<_, String>("job_kind")?),
        native_job: row.get("native_job")?,
        prompt_kind: BackgroundPromptKind::parse(&row.get::<_, String>("prompt_kind")?),
        prompt: row.get("prompt")?,
        context_url: row.get("context_url")?,
        persona: row.get("persona")?,
        agent_folder: row.get("agent_folder")?,
        workspace_dir: row.get("workspace_dir")?,
        use_tools,
        mcp_json: row.get("mcp")?,
        model_id: row.get("model_id")?,
        max_turns: row.get("max_turns")?,
        timeout_secs: row.get("timeout_secs")?,
        continuity: BackgroundContinuity::parse(&row.get::<_, String>("continuity")?),
        memory_folder: row.get("memory_folder")?,
        trigger_type: BackgroundTrigger::parse(&row.get::<_, String>("trigger_type")?),
        trigger_value: row.get("trigger_value")?,
        next_run: row.get("next_run")?,
        last_run: row.get("last_run")?,
        overlap_policy: OverlapPolicy::parse(&row.get::<_, String>("overlap_policy")?),
        catch_up: row.get::<_, i64>("catch_up")? != 0,
        max_failures: row.get("max_failures")?,
        consecutive_failures: row.get("consecutive_failures")?,
        visibility: BackgroundVisibility::parse(&row.get::<_, String>("visibility")?),
        notify: row.get::<_, i64>("notify")? != 0,
        status: BackgroundTaskStatus::parse(&row.get::<_, String>("status")?),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn row_to_background_run(row: &Row<'_>) -> rusqlite::Result<BackgroundRun> {
    Ok(BackgroundRun {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        session_id: row.get("session_id")?,
        trigger_kind: BackgroundTriggerKind::parse(&row.get::<_, String>("trigger_kind")?),
        status: BackgroundRunStatus::parse(&row.get::<_, String>("status")?),
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        duration_ms: row.get("duration_ms")?,
        turn_count: row.get("turn_count")?,
        tokens_in: row.get("tokens_in")?,
        tokens_out: row.get("tokens_out")?,
        prompt: row.get("prompt")?,
        result: row.get("result")?,
        error: row.get("error")?,
    })
}

impl super::Db {
    // ============================================================
    // Tasks
    // ============================================================

    /// Insert or update by `(owner_id, owner_key)`.
    ///
    /// The upsert is what makes App reinstall idempotent. It deliberately
    /// preserves the live columns — `status`, `next_run`, `last_run`,
    /// `consecutive_failures` — so reinstalling an App does not silently
    /// re-enable a task the user paused, or wipe its failure history.
    pub fn upsert_background_task(&self, task: &BackgroundTask) -> Result<()> {
        let use_tools = serde_json::to_string(&task.use_tools)?;
        self.with_conn(|c| {
            c.execute(
                r#"
                INSERT INTO background_tasks
                  (id, owner_kind, owner_id, owner_key, title, description,
                   job_kind, native_job, prompt_kind, prompt, context_url,
                   persona, agent_folder, workspace_dir, use_tools, mcp, model_id,
                   max_turns, timeout_secs, continuity, memory_folder,
                   trigger_type, trigger_value, next_run, last_run,
                   overlap_policy, catch_up, max_failures, consecutive_failures,
                   visibility, notify, status, created_at, updated_at)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,
                        ?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,?33,?34)
                ON CONFLICT(owner_id, owner_key) DO UPDATE SET
                  title         = excluded.title,
                  description   = excluded.description,
                  job_kind      = excluded.job_kind,
                  native_job    = excluded.native_job,
                  prompt_kind   = excluded.prompt_kind,
                  prompt        = excluded.prompt,
                  context_url   = excluded.context_url,
                  persona       = excluded.persona,
                  agent_folder  = excluded.agent_folder,
                  workspace_dir = excluded.workspace_dir,
                  use_tools     = excluded.use_tools,
                  mcp           = excluded.mcp,
                  model_id      = excluded.model_id,
                  max_turns     = excluded.max_turns,
                  timeout_secs  = excluded.timeout_secs,
                  continuity    = excluded.continuity,
                  memory_folder = excluded.memory_folder,
                  trigger_type  = excluded.trigger_type,
                  trigger_value = excluded.trigger_value,
                  overlap_policy= excluded.overlap_policy,
                  catch_up      = excluded.catch_up,
                  max_failures  = excluded.max_failures,
                  visibility    = excluded.visibility,
                  notify        = excluded.notify,
                  updated_at    = excluded.updated_at
                "#,
                params![
                    task.id,
                    task.owner_kind.as_str(),
                    task.owner_id,
                    task.owner_key,
                    task.title,
                    task.description,
                    task.job_kind.as_str(),
                    task.native_job,
                    task.prompt_kind.as_str(),
                    task.prompt,
                    task.context_url,
                    task.persona,
                    task.agent_folder,
                    task.workspace_dir,
                    use_tools,
                    task.mcp_json,
                    task.model_id,
                    task.max_turns,
                    task.timeout_secs,
                    task.continuity.as_str(),
                    task.memory_folder,
                    task.trigger_type.as_str(),
                    task.trigger_value,
                    task.next_run,
                    task.last_run,
                    task.overlap_policy.as_str(),
                    task.catch_up as i64,
                    task.max_failures,
                    task.consecutive_failures,
                    task.visibility.as_str(),
                    task.notify as i64,
                    task.status.as_str(),
                    task.created_at,
                    task.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_background_task(&self, id: &str) -> Result<Option<BackgroundTask>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT * FROM background_tasks WHERE id = ?1")?;
            let mut rows = stmt.query_map(params![id], row_to_background_task)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    pub fn get_background_task_by_key(
        &self,
        owner_id: &str,
        owner_key: &str,
    ) -> Result<Option<BackgroundTask>> {
        self.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT * FROM background_tasks WHERE owner_id = ?1 AND owner_key = ?2")?;
            let mut rows = stmt.query_map(params![owner_id, owner_key], row_to_background_task)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// Due tasks: active, with a `next_run` in the past.
    ///
    /// Ordered by `next_run` so the longest-overdue goes first, but unlike the
    /// scheduled-task loop the caller is expected to spawn these concurrently
    /// rather than await each in turn.
    pub fn get_due_background_tasks(&self, now: &str) -> Result<Vec<BackgroundTask>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM background_tasks
                 WHERE status = 'active' AND next_run IS NOT NULL AND next_run <= ?1
                 ORDER BY next_run ASC",
            )?;
            let rows = stmt
                .query_map(params![now], row_to_background_task)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn list_background_tasks(&self, f: &BackgroundTaskFilter) -> Result<Vec<BackgroundTask>> {
        let mut sql = String::from("SELECT * FROM background_tasks WHERE 1=1");
        let mut args: Vec<String> = Vec::new();
        if let Some(k) = &f.owner_kind {
            sql.push_str(" AND owner_kind = ?");
            args.push(k.clone());
        }
        if let Some(o) = &f.owner_id {
            sql.push_str(" AND owner_id = ?");
            args.push(o.clone());
        }
        if let Some(s) = &f.status {
            sql.push_str(" AND status = ?");
            args.push(s.clone());
        }
        if !f.include_internal {
            sql.push_str(" AND visibility != 'internal'");
        }
        sql.push_str(" ORDER BY created_at DESC");
        if let Some(limit) = f.limit {
            sql.push_str(&format!(" LIMIT {}", limit.max(0)));
            if let Some(offset) = f.offset {
                sql.push_str(&format!(" OFFSET {}", offset.max(0)));
            }
        }
        self.with_conn(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(args.iter()), row_to_background_task)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Count of tasks matching a filter, ignoring limit/offset — the total the
    /// pager needs.
    pub fn count_background_tasks(&self, f: &BackgroundTaskFilter) -> Result<i64> {
        let mut sql = String::from("SELECT COUNT(*) FROM background_tasks WHERE 1=1");
        let mut args: Vec<String> = Vec::new();
        if let Some(k) = &f.owner_kind {
            sql.push_str(" AND owner_kind = ?");
            args.push(k.clone());
        }
        if let Some(o) = &f.owner_id {
            sql.push_str(" AND owner_id = ?");
            args.push(o.clone());
        }
        if let Some(s) = &f.status {
            sql.push_str(" AND status = ?");
            args.push(s.clone());
        }
        if !f.include_internal {
            sql.push_str(" AND visibility != 'internal'");
        }
        self.with_conn(|c| {
            let n: i64 = c.query_row(&sql, params_from_iter(args.iter()), |r| r.get(0))?;
            Ok(n)
        })
    }

    /// Advance `next_run` + `status` ahead of execution, so a slow run can't be
    /// picked up twice by the next tick.
    pub fn advance_background_next_run(
        &self,
        id: &str,
        next_run: Option<&str>,
        status: BackgroundTaskStatus,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE background_tasks
                 SET next_run = ?1, status = ?2, updated_at = ?3
                 WHERE id = ?4",
                params![next_run, status.as_str(), now_rfc3339(), id],
            )?;
            Ok(())
        })
    }

    pub fn set_background_task_status(&self, id: &str, status: BackgroundTaskStatus) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE background_tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status.as_str(), now_rfc3339(), id],
            )?;
            Ok(())
        })
    }

    /// Resume: clear the failure counter and re-arm `next_run`.
    pub fn resume_background_task(&self, id: &str, next_run: Option<&str>) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE background_tasks
                 SET status = 'active', consecutive_failures = 0, next_run = ?1, updated_at = ?2
                 WHERE id = ?3",
                params![next_run, now_rfc3339(), id],
            )?;
            Ok(())
        })
    }

    pub fn mark_background_task_run(&self, id: &str, last_run: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE background_tasks SET last_run = ?1, updated_at = ?2 WHERE id = ?3",
                params![last_run, now_rfc3339(), id],
            )?;
            Ok(())
        })
    }

    /// Count a failure and auto-quarantine at `max_failures`.
    ///
    /// Returns `true` when this failure tipped the task into `failed`, so the
    /// caller can emit the WS event and surface it in `attention` exactly once.
    /// `max_failures = 0` disables quarantine.
    pub fn record_background_failure(&self, id: &str) -> Result<bool> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE background_tasks
                 SET consecutive_failures = consecutive_failures + 1, updated_at = ?1
                 WHERE id = ?2",
                params![now_rfc3339(), id],
            )?;
            let (fails, max, status): (i64, i64, String) = c.query_row(
                "SELECT consecutive_failures, max_failures, status
                 FROM background_tasks WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let should_quarantine = max > 0 && fails >= max && status == "active";
            if should_quarantine {
                c.execute(
                    "UPDATE background_tasks SET status = 'failed', updated_at = ?1 WHERE id = ?2",
                    params![now_rfc3339(), id],
                )?;
            }
            Ok(should_quarantine)
        })
    }

    pub fn reset_background_failures(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE background_tasks
                 SET consecutive_failures = 0, updated_at = ?1
                 WHERE id = ?2",
                params![now_rfc3339(), id],
            )?;
            Ok(())
        })
    }

    pub fn delete_background_task(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM background_tasks WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    /// Remove every task an App owns. Called on uninstall.
    ///
    /// Runs are deliberately kept — same rationale as `task_run_logs` surviving
    /// `delete_task`: the history is an audit trail, and the user may well be
    /// uninstalling *because* of what it says. Retention prunes them later.
    pub fn delete_background_tasks_by_owner(
        &self,
        owner_kind: BackgroundOwnerKind,
        owner_id: &str,
    ) -> Result<usize> {
        self.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM background_tasks WHERE owner_kind = ?1 AND owner_id = ?2",
                params![owner_kind.as_str(), owner_id],
            )?;
            Ok(n)
        })
    }

    /// Quota check (design §10 guard 4).
    pub fn count_background_tasks_by_owner(
        &self,
        owner_id: &str,
        active_only: bool,
    ) -> Result<i64> {
        self.with_conn(|c| {
            let n: i64 = if active_only {
                c.query_row(
                    "SELECT COUNT(*) FROM background_tasks WHERE owner_id = ?1 AND status = 'active'",
                    params![owner_id],
                    |r| r.get(0),
                )?
            } else {
                c.query_row(
                    "SELECT COUNT(*) FROM background_tasks WHERE owner_id = ?1",
                    params![owner_id],
                    |r| r.get(0),
                )?
            };
            Ok(n)
        })
    }

    // ============================================================
    // Runs
    // ============================================================

    pub fn insert_background_run(&self, run: &BackgroundRun) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                r#"
                INSERT INTO background_runs
                  (id, task_id, session_id, trigger_kind, status, started_at,
                   finished_at, duration_ms, turn_count, tokens_in, tokens_out,
                   prompt, result, error)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                "#,
                params![
                    run.id,
                    run.task_id,
                    run.session_id,
                    run.trigger_kind.as_str(),
                    run.status.as_str(),
                    run.started_at,
                    run.finished_at,
                    run.duration_ms,
                    run.turn_count,
                    run.tokens_in,
                    run.tokens_out,
                    run.prompt,
                    run.result,
                    run.error,
                ],
            )?;
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn finish_background_run(
        &self,
        id: &str,
        status: BackgroundRunStatus,
        prompt: Option<&str>,
        result: Option<&str>,
        error: Option<&str>,
        duration_ms: i64,
        turn_count: Option<i64>,
        tokens: Option<(i64, i64)>,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE background_runs
                 SET status = ?1, prompt = COALESCE(?2, prompt), result = ?3, error = ?4,
                     duration_ms = ?5, turn_count = ?6, finished_at = ?7,
                     tokens_in = COALESCE(?9, tokens_in),
                     tokens_out = COALESCE(?10, tokens_out)
                 WHERE id = ?8",
                params![
                    status.as_str(),
                    prompt,
                    result,
                    error,
                    duration_ms,
                    turn_count,
                    now_rfc3339(),
                    id,
                    tokens.map(|t| t.0),
                    tokens.map(|t| t.1),
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_background_run(&self, id: &str) -> Result<Option<BackgroundRun>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT * FROM background_runs WHERE id = ?1")?;
            let mut rows = stmt.query_map(params![id], row_to_background_run)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    pub fn list_background_runs(&self, task_id: &str, limit: i64) -> Result<Vec<BackgroundRun>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM background_runs WHERE task_id = ?1
                 ORDER BY started_at DESC LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![task_id, limit], row_to_background_run)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn recent_background_runs(&self, limit: i64) -> Result<Vec<BackgroundRun>> {
        self.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT * FROM background_runs ORDER BY started_at DESC LIMIT ?1")?;
            let rows = stmt
                .query_map(params![limit], row_to_background_run)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Any run still marked `running` at boot is a leftover from a crash — the
    /// process that owned it is gone, so nothing will ever finish it.
    pub fn reclaim_orphan_background_runs(&self) -> Result<usize> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE background_runs
                 SET status = 'error', error = 'daemon stopped while this run was in flight',
                     finished_at = ?1
                 WHERE status = 'running'",
                params![now_rfc3339()],
            )?;
            Ok(n)
        })
    }

    /// Retention: drop runs older than `keep_days`, and their activity.
    pub fn prune_background_runs(&self, keep_days: i64) -> Result<usize> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(keep_days.max(1))).to_rfc3339();
        self.with_conn(|c| {
            c.execute(
                "DELETE FROM background_activity WHERE run_id IN
                   (SELECT id FROM background_runs WHERE started_at < ?1)",
                params![cutoff],
            )?;
            let n = c.execute(
                "DELETE FROM background_runs WHERE started_at < ?1",
                params![cutoff],
            )?;
            Ok(n)
        })
    }

    // ============================================================
    // Activity (the background-session transcript)
    // ============================================================

    pub fn insert_background_activity(&self, run_id: &str, kind: &str, detail: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO background_activity (run_id, ts, kind, detail) VALUES (?1,?2,?3,?4)",
                params![run_id, now_rfc3339(), kind, detail],
            )?;
            Ok(())
        })
    }

    pub fn get_background_activity(
        &self,
        run_id: &str,
        limit: i64,
    ) -> Result<Vec<BackgroundActivity>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM background_activity WHERE run_id = ?1 ORDER BY id ASC LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![run_id, limit], |r| {
                    Ok(BackgroundActivity {
                        id: r.get("id")?,
                        run_id: r.get("run_id")?,
                        ts: r.get("ts")?,
                        kind: r.get("kind")?,
                        detail: r.get("detail")?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // ============================================================
    // Statistics
    // ============================================================

    /// Aggregate run outcomes since `since` (RFC3339), optionally for one owner.
    pub fn background_totals(
        &self,
        since: &str,
        owner_id: Option<&str>,
    ) -> Result<BackgroundTotals> {
        self.with_conn(|c| {
            let (sql_filter, args): (&str, Vec<String>) = match owner_id {
                Some(o) => (
                    " AND task_id IN (SELECT id FROM background_tasks WHERE owner_id = ?2)",
                    vec![since.to_owned(), o.to_owned()],
                ),
                None => ("", vec![since.to_owned()]),
            };

            let base = format!(
                "SELECT
                   COUNT(*),
                   COALESCE(SUM(status = 'success'), 0),
                   COALESCE(SUM(status = 'error'), 0),
                   COALESCE(SUM(status = 'timeout'), 0),
                   COALESCE(SUM(status = 'cancelled'), 0),
                   COALESCE(SUM(status = 'skipped'), 0),
                   COALESCE(SUM(status = 'running'), 0),
                   COALESCE(AVG(duration_ms), 0),
                   COALESCE(SUM(tokens_in), 0),
                   COALESCE(SUM(tokens_out), 0)
                 FROM background_runs WHERE started_at >= ?1{sql_filter}"
            );
            let mut t = c.query_row(&base, params_from_iter(args.iter()), |r| {
                Ok(BackgroundTotals {
                    runs: r.get(0)?,
                    success: r.get(1)?,
                    error: r.get(2)?,
                    timeout: r.get(3)?,
                    cancelled: r.get(4)?,
                    skipped: r.get(5)?,
                    running: r.get(6)?,
                    success_rate: 0.0,
                    avg_duration_ms: r.get::<_, f64>(7)? as i64,
                    p95_duration_ms: 0,
                    tokens_in: r.get(8)?,
                    tokens_out: r.get(9)?,
                })
            })?;

            // Success rate excludes skips: a `template` task that skips because
            // there is nothing to do is healthy, and folding skips into either
            // bucket distorts the number the user is judging the task by.
            let judged = t.runs - t.skipped - t.running;
            t.success_rate = if judged > 0 {
                t.success as f64 / judged as f64
            } else {
                1.0
            };

            // p95 by offset — one extra query, exact, and fine at these volumes.
            let p95_sql = format!(
                "SELECT duration_ms FROM background_runs
                 WHERE started_at >= ?1 AND duration_ms IS NOT NULL{sql_filter}
                 ORDER BY duration_ms
                 LIMIT 1 OFFSET (
                   SELECT CAST(COUNT(*) * 0.95 AS INTEGER) FROM background_runs
                   WHERE started_at >= ?1 AND duration_ms IS NOT NULL{sql_filter}
                 )"
            );
            t.p95_duration_ms = c
                .query_row(&p95_sql, params_from_iter(args.iter()), |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap_or(0);

            Ok(t)
        })
    }

    /// Per-task rollup for the stats view.
    pub fn background_task_stats(&self, since: &str) -> Result<Vec<BackgroundTaskStats>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT
                   t.id, t.title, t.owner_kind, t.owner_id, t.status, t.next_run,
                   t.consecutive_failures,
                   COUNT(r.id),
                   COALESCE(SUM(r.status = 'success'), 0),
                   COALESCE(SUM(r.status = 'skipped'), 0),
                   COALESCE(SUM(r.status IN ('error','timeout')), 0),
                   COALESCE(AVG(r.duration_ms), 0)
                 FROM background_tasks t
                 LEFT JOIN background_runs r ON r.task_id = t.id AND r.started_at >= ?1
                 GROUP BY t.id
                 ORDER BY t.created_at DESC",
            )?;
            let rows = stmt
                .query_map(params![since], |r| {
                    let runs: i64 = r.get(7)?;
                    let success: i64 = r.get(8)?;
                    let skipped: i64 = r.get(9)?;
                    let judged = runs - skipped;
                    Ok(BackgroundTaskStats {
                        task_id: r.get(0)?,
                        title: r.get(1)?,
                        owner_kind: r.get(2)?,
                        owner_id: r.get(3)?,
                        status: r.get(4)?,
                        next_run: r.get(5)?,
                        consecutive_failures: r.get(6)?,
                        runs,
                        success,
                        skipped,
                        failures: r.get(10)?,
                        success_rate: if judged > 0 {
                            success as f64 / judged as f64
                        } else {
                            1.0
                        },
                        avg_duration_ms: r.get::<_, f64>(11)? as i64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Tasks the user should look at: auto-quarantined, or currently failing.
    pub fn background_attention(&self) -> Result<Vec<BackgroundAttention>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT t.id, t.title, t.status, t.consecutive_failures,
                        (SELECT error FROM background_runs
                          WHERE task_id = t.id AND error IS NOT NULL
                          ORDER BY started_at DESC LIMIT 1)
                 FROM background_tasks t
                 WHERE t.status = 'failed' OR t.consecutive_failures > 0
                 ORDER BY t.consecutive_failures DESC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(BackgroundAttention {
                        task_id: r.get(0)?,
                        title: r.get(1)?,
                        status: r.get(2)?,
                        consecutive_failures: r.get(3)?,
                        last_error: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundTotals {
    pub runs: i64,
    pub success: i64,
    pub error: i64,
    pub timeout: i64,
    pub cancelled: i64,
    pub skipped: i64,
    pub running: i64,
    /// Excludes skipped and in-flight runs from the denominator.
    pub success_rate: f64,
    pub avg_duration_ms: i64,
    pub p95_duration_ms: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundTaskStats {
    pub task_id: String,
    pub title: String,
    pub owner_kind: String,
    pub owner_id: String,
    pub status: String,
    pub next_run: Option<String>,
    pub consecutive_failures: i64,
    pub runs: i64,
    pub success: i64,
    pub skipped: i64,
    pub failures: i64,
    pub success_rate: f64,
    pub avg_duration_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackgroundAttention {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub consecutive_failures: i64,
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::types::{
        BackgroundContinuity, BackgroundJobKind, BackgroundPromptKind, BackgroundTriggerKind,
        BackgroundVisibility, OverlapPolicy,
    };

    fn db() -> super::super::Db {
        super::super::Db::open_in_memory(&Config::from_env()).unwrap()
    }

    fn task(owner_kind: BackgroundOwnerKind, owner_id: &str, key: &str) -> BackgroundTask {
        BackgroundTask {
            id: uuid::Uuid::new_v4().to_string(),
            owner_kind,
            owner_id: owner_id.into(),
            owner_key: key.into(),
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
            use_tools: vec!["Read".into()],
            mcp_json: None,
            model_id: None,
            max_turns: None,
            timeout_secs: None,
            continuity: BackgroundContinuity::Fresh,
            memory_folder: None,
            trigger_type: BackgroundTrigger::Cron,
            trigger_value: Some("0 9 * * *".into()),
            next_run: Some("2020-01-01T00:00:00Z".into()),
            last_run: None,
            overlap_policy: OverlapPolicy::Skip,
            catch_up: false,
            max_failures: 3,
            consecutive_failures: 0,
            visibility: BackgroundVisibility::Normal,
            notify: false,
            status: BackgroundTaskStatus::Active,
            created_at: "2026-07-17T00:00:00Z".into(),
            updated_at: "2026-07-17T00:00:00Z".into(),
        }
    }

    fn run(task_id: &str, status: BackgroundRunStatus, dur: i64) -> BackgroundRun {
        let id = uuid::Uuid::new_v4().to_string();
        BackgroundRun {
            session_id: format!("bg:{id}"),
            id,
            task_id: task_id.into(),
            trigger_kind: BackgroundTriggerKind::Schedule,
            status,
            started_at: chrono::Utc::now().to_rfc3339(),
            finished_at: None,
            duration_ms: Some(dur),
            turn_count: None,
            tokens_in: Some(10),
            tokens_out: Some(5),
            prompt: None,
            result: None,
            error: (status == BackgroundRunStatus::Error).then(|| "boom".to_owned()),
        }
    }

    #[test]
    fn reinstall_updates_config_but_never_resurrects_a_paused_task() {
        // The whole point of the (owner_id, owner_key) upsert: re-installing an
        // app must not silently re-enable a task the user deliberately paused,
        // nor wipe the failure history that explains why.
        let db = db();
        let mut t = task(BackgroundOwnerKind::App, "crm", "daily-followup");
        db.upsert_background_task(&t).unwrap();

        db.set_background_task_status(&t.id, BackgroundTaskStatus::Paused)
            .unwrap();
        db.record_background_failure(&t.id).unwrap();

        // Reinstall: same key, new id and new prompt.
        t.id = uuid::Uuid::new_v4().to_string();
        t.prompt = Some("v2 prompt".into());
        t.title = "v2".into();
        db.upsert_background_task(&t).unwrap();

        let all = db
            .list_background_tasks(&BackgroundTaskFilter::default())
            .unwrap();
        assert_eq!(all.len(), 1, "upsert must not duplicate on reinstall");
        let got = &all[0];
        assert_eq!(got.prompt.as_deref(), Some("v2 prompt"), "config updates");
        assert_eq!(got.title, "v2");
        assert_eq!(
            got.status,
            BackgroundTaskStatus::Paused,
            "a paused task must stay paused across reinstall"
        );
        assert_eq!(got.consecutive_failures, 1, "failure history survives");
    }

    #[test]
    fn failures_quarantine_exactly_at_the_limit_and_announce_once() {
        let db = db();
        let t = task(BackgroundOwnerKind::User, "main", "k"); // max_failures = 3
        db.upsert_background_task(&t).unwrap();

        assert!(!db.record_background_failure(&t.id).unwrap());
        assert!(!db.record_background_failure(&t.id).unwrap());
        assert!(
            db.record_background_failure(&t.id).unwrap(),
            "third failure must quarantine and report it"
        );
        assert_eq!(
            db.get_background_task(&t.id).unwrap().unwrap().status,
            BackgroundTaskStatus::Failed
        );
        // Already quarantined: must not re-announce, or the UI gets a storm.
        assert!(!db.record_background_failure(&t.id).unwrap());
    }

    #[test]
    fn max_failures_zero_disables_quarantine() {
        let db = db();
        let mut t = task(BackgroundOwnerKind::System, "core.cognitive", "decay");
        t.max_failures = 0;
        db.upsert_background_task(&t).unwrap();
        for _ in 0..10 {
            assert!(!db.record_background_failure(&t.id).unwrap());
        }
        assert_eq!(
            db.get_background_task(&t.id).unwrap().unwrap().status,
            BackgroundTaskStatus::Active,
            "max_failures=0 means never auto-pause"
        );
    }

    #[test]
    fn only_active_tasks_with_a_past_next_run_are_due() {
        let db = db();
        let due = task(BackgroundOwnerKind::User, "main", "due");
        db.upsert_background_task(&due).unwrap();

        let mut paused = task(BackgroundOwnerKind::User, "main", "paused");
        paused.status = BackgroundTaskStatus::Paused;
        db.upsert_background_task(&paused).unwrap();

        let mut future = task(BackgroundOwnerKind::User, "main", "future");
        future.next_run = Some("2099-01-01T00:00:00Z".into());
        db.upsert_background_task(&future).unwrap();

        let mut manual = task(BackgroundOwnerKind::User, "main", "manual");
        manual.next_run = None;
        db.upsert_background_task(&manual).unwrap();

        let got = db
            .get_due_background_tasks(&chrono::Utc::now().to_rfc3339())
            .unwrap();
        assert_eq!(got.len(), 1, "expected only the overdue active task");
        assert_eq!(got[0].owner_key, "due");
    }

    #[test]
    fn internal_tasks_are_hidden_unless_asked_for() {
        let db = db();
        db.upsert_background_task(&task(BackgroundOwnerKind::User, "main", "mine"))
            .unwrap();
        let mut internal = task(BackgroundOwnerKind::System, "core.cognitive", "decay");
        internal.visibility = BackgroundVisibility::Internal;
        db.upsert_background_task(&internal).unwrap();

        let default = db
            .list_background_tasks(&BackgroundTaskFilter::default())
            .unwrap();
        assert_eq!(
            default.len(),
            1,
            "core upkeep must not bury the user's tasks"
        );

        let with_internal = db
            .list_background_tasks(&BackgroundTaskFilter {
                include_internal: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(with_internal.len(), 2);
    }

    #[test]
    fn uninstall_removes_an_apps_tasks_but_keeps_the_audit_trail() {
        let db = db();
        let crm = task(BackgroundOwnerKind::App, "crm", "followup");
        let mine = task(BackgroundOwnerKind::User, "main", "mine");
        db.upsert_background_task(&crm).unwrap();
        db.upsert_background_task(&mine).unwrap();
        db.insert_background_run(&run(&crm.id, BackgroundRunStatus::Error, 10))
            .unwrap();

        let n = db
            .delete_background_tasks_by_owner(BackgroundOwnerKind::App, "crm")
            .unwrap();
        assert_eq!(n, 1);
        assert!(db.get_background_task(&crm.id).unwrap().is_none());
        assert!(
            db.get_background_task(&mine.id).unwrap().is_some(),
            "uninstalling one app must not touch anything else"
        );
        assert_eq!(
            db.list_background_runs(&crm.id, 10).unwrap().len(),
            1,
            "runs are the audit trail — the user may be uninstalling because of them"
        );
    }

    #[test]
    fn success_rate_ignores_skips_so_quiet_tasks_dont_look_broken() {
        let db = db();
        let t = task(BackgroundOwnerKind::App, "crm", "followup");
        db.upsert_background_task(&t).unwrap();
        // 2 success, 1 error, 7 skips: a template task that mostly has nothing
        // to do. Judged on real outcomes that's 2/3, not 2/10.
        for _ in 0..2 {
            db.insert_background_run(&run(&t.id, BackgroundRunStatus::Success, 100))
                .unwrap();
        }
        db.insert_background_run(&run(&t.id, BackgroundRunStatus::Error, 100))
            .unwrap();
        for _ in 0..7 {
            db.insert_background_run(&run(&t.id, BackgroundRunStatus::Skipped, 0))
                .unwrap();
        }

        let since = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let totals = db.background_totals(&since, None).unwrap();
        assert_eq!(totals.runs, 10);
        assert_eq!(totals.skipped, 7);
        assert!(
            (totals.success_rate - 2.0 / 3.0).abs() < 1e-9,
            "success_rate should be 2/3, got {}",
            totals.success_rate
        );
        assert_eq!(totals.tokens_in, 100, "tokens summed across all runs");
    }

    #[test]
    fn totals_scope_to_one_owner_when_asked() {
        let db = db();
        let crm = task(BackgroundOwnerKind::App, "crm", "a");
        let mine = task(BackgroundOwnerKind::User, "main", "b");
        db.upsert_background_task(&crm).unwrap();
        db.upsert_background_task(&mine).unwrap();
        db.insert_background_run(&run(&crm.id, BackgroundRunStatus::Success, 10))
            .unwrap();
        db.insert_background_run(&run(&mine.id, BackgroundRunStatus::Error, 20))
            .unwrap();

        let since = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        assert_eq!(db.background_totals(&since, None).unwrap().runs, 2);
        let crm_only = db.background_totals(&since, Some("crm")).unwrap();
        assert_eq!(crm_only.runs, 1);
        assert_eq!(crm_only.success, 1);
    }

    #[test]
    fn empty_history_reads_as_healthy_not_as_zero_percent() {
        // A brand-new task has no runs. 0/0 must not render as "0% success" and
        // land it in the attention band on day one.
        let db = db();
        let since = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let totals = db.background_totals(&since, None).unwrap();
        assert_eq!(totals.runs, 0);
        assert_eq!(totals.success_rate, 1.0);
        assert_eq!(totals.avg_duration_ms, 0);
    }

    #[test]
    fn attention_surfaces_failing_tasks_with_their_last_error() {
        let db = db();
        let healthy = task(BackgroundOwnerKind::User, "main", "ok");
        let broken = task(BackgroundOwnerKind::User, "main", "bad");
        db.upsert_background_task(&healthy).unwrap();
        db.upsert_background_task(&broken).unwrap();
        db.insert_background_run(&run(&broken.id, BackgroundRunStatus::Error, 5))
            .unwrap();
        db.record_background_failure(&broken.id).unwrap();

        let att = db.background_attention().unwrap();
        assert_eq!(att.len(), 1, "a healthy task must not appear in attention");
        assert_eq!(att[0].task_id, broken.id);
        assert_eq!(att[0].last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn success_resets_the_failure_counter() {
        let db = db();
        let t = task(BackgroundOwnerKind::User, "main", "k");
        db.upsert_background_task(&t).unwrap();
        db.record_background_failure(&t.id).unwrap();
        db.record_background_failure(&t.id).unwrap();
        db.reset_background_failures(&t.id).unwrap();
        assert_eq!(
            db.get_background_task(&t.id)
                .unwrap()
                .unwrap()
                .consecutive_failures,
            0
        );
        assert!(db.background_attention().unwrap().is_empty());
    }

    #[test]
    fn orphan_runs_are_reclaimed_rather_than_hanging_forever() {
        let db = db();
        let t = task(BackgroundOwnerKind::User, "main", "k");
        db.upsert_background_task(&t).unwrap();
        db.insert_background_run(&run(&t.id, BackgroundRunStatus::Running, 0))
            .unwrap();
        db.insert_background_run(&run(&t.id, BackgroundRunStatus::Success, 5))
            .unwrap();

        assert_eq!(db.reclaim_orphan_background_runs().unwrap(), 1);
        let runs = db.list_background_runs(&t.id, 10).unwrap();
        assert!(
            runs.iter()
                .all(|r| r.status != BackgroundRunStatus::Running),
            "no run may stay 'running' after the owning process died"
        );
        assert_eq!(
            runs.iter()
                .filter(|r| r.status == BackgroundRunStatus::Success)
                .count(),
            1,
            "a finished run must not be rewritten"
        );
    }

    #[test]
    fn use_tools_survives_the_json_round_trip() {
        let db = db();
        let mut t = task(BackgroundOwnerKind::User, "main", "k");
        t.use_tools = vec!["mcp__crm-mcp__crm_customer_list".into(), "Read".into()];
        db.upsert_background_task(&t).unwrap();
        let got = db.get_background_task(&t.id).unwrap().unwrap();
        assert_eq!(got.use_tools, t.use_tools);
    }

    #[test]
    fn quota_counts_total_and_active_separately() {
        let db = db();
        db.upsert_background_task(&task(BackgroundOwnerKind::User, "main", "a"))
            .unwrap();
        let mut paused = task(BackgroundOwnerKind::User, "main", "b");
        paused.status = BackgroundTaskStatus::Paused;
        db.upsert_background_task(&paused).unwrap();
        db.upsert_background_task(&task(BackgroundOwnerKind::App, "crm", "c"))
            .unwrap();

        assert_eq!(
            db.count_background_tasks_by_owner("main", false).unwrap(),
            2
        );
        assert_eq!(db.count_background_tasks_by_owner("main", true).unwrap(), 1);
        assert_eq!(db.count_background_tasks_by_owner("crm", false).unwrap(), 1);
    }
}

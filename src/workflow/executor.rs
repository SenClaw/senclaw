//! WorkflowExecutor — DAG scheduling engine.
//!
//! Port of `SemaClaw/src/workflow/WorkflowExecutor.ts`.
//!
//! Fully standalone: only spawns isolated sessions (agent) or child
//! processes (script) via `step_runners` — never touches AgentPool /
//! DispatchBridge / persistent agents.
//!
//! Entry points:
//!   - `run(def, inputs)`: awaits completion, returns the final run (CLI).
//!   - `start(def, inputs)`: returns `(run, JoinHandle)` immediately with a
//!     run id; progresses in the background (daemon, with `on_update` → WS).
//!   - `cancel(run_id)`: stop dispatching new steps, mark cancelled, and
//!     abort in-flight steps (kill script process group / abort agent session).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::agent::persona_registry::{PersonaConfig, PersonaRegistry};

use super::run_store::WorkflowRunStore;
use super::settings::LiveWorkflowSettings;
use super::step_runners::{run_agent_step, run_script_step, StepRunResult};
use super::types::{
    now_iso, sanitize_name, ObserveAs, ObserveFrom, ObserveOutput, ObserveSpec, RenderContext,
    RunStatus, StepKind, StepRun, StepStatus, WorkflowDef, WorkflowRun,
};

const DEFAULT_CONCURRENCY: usize = 5;

/// Live-activity cap per run (oldest entries dropped) and per-entry text cap.
const ACTIVITY_MAX_ENTRIES: usize = 300;
const ACTIVITY_MAX_TEXT: usize = 8000;
/// Activity buffers kept for at most this many runs (oldest evicted).
const ACTIVITY_MAX_RUNS: usize = 20;

/// One live-activity line of an agent step (thinking, tool call, message).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub ts: String,
    pub step_id: String,
    /// think | text | tool | tool_error | message | status
    pub kind: String,
    pub text: String,
}

/// Callback fired after every persisted state change (daemon → WS push).
pub type OnUpdate = Arc<dyn Fn(&WorkflowRun) + Send + Sync>;

pub struct WorkflowExecutorOpts {
    pub store: Arc<WorkflowRunStore>,
    /// Persona lookup (shared registry).
    pub persona_registry: Arc<Mutex<PersonaRegistry>>,
    /// Per-run concurrency cap, default 5.
    pub concurrency: Option<usize>,
    /// Fired after each persisted state change.
    pub on_update: Option<OnUpdate>,
    /// Default workspace root: workflows without a custom `workspace` use
    /// `<root>/<sanitized-name>/`.
    pub workflow_data_dir: PathBuf,
    /// Extra skills dirs handed to agent-step sessions (bundled + managed;
    /// `<workspace>/skills` is appended per run).
    pub skills_extra_dirs: Vec<String>,
    /// Extra MCP servers injected into agent-step sessions (e.g. browser-mcp,
    /// mirroring VirtualWorkerPool) so web personas have their tools.
    pub extra_mcp_servers: Vec<crate::zen_core::McpServerConfig>,
    /// POSIX shell override for script steps (config `SENCLAW_WORKFLOW_SHELL`).
    pub shell_override: Option<String>,
    /// Live tunables: agent-step (LLM) parallelism + no-result retries.
    pub settings: LiveWorkflowSettings,
}

struct RunControl {
    cancel: CancellationToken,
}

pub struct WorkflowExecutor {
    store: Arc<WorkflowRunStore>,
    persona_registry: Arc<Mutex<PersonaRegistry>>,
    concurrency: usize,
    on_update: Option<OnUpdate>,
    workflow_data_dir: PathBuf,
    skills_extra_dirs: Vec<String>,
    extra_mcp_servers: Vec<crate::zen_core::McpServerConfig>,
    shell_override: Option<String>,
    settings: LiveWorkflowSettings,
    /// In-flight runs: run id → control (for cancel).
    active_runs: Mutex<HashMap<String, Arc<RunControl>>>,
    /// Workspaces occupied by in-flight runs (same dir must not run twice).
    active_workspaces: Mutex<HashSet<String>>,
    /// Live-activity feed per run (in-memory; UI polls it). Insertion order
    /// tracked separately for cheap eviction of the oldest run.
    activity: Mutex<(Vec<String>, HashMap<String, Vec<ActivityEntry>>)>,
}

impl WorkflowExecutor {
    pub fn new(opts: WorkflowExecutorOpts) -> Arc<Self> {
        Arc::new(Self {
            store: opts.store,
            persona_registry: opts.persona_registry,
            concurrency: opts.concurrency.unwrap_or(DEFAULT_CONCURRENCY).max(1),
            on_update: opts.on_update,
            workflow_data_dir: opts.workflow_data_dir,
            skills_extra_dirs: opts.skills_extra_dirs,
            extra_mcp_servers: opts.extra_mcp_servers,
            shell_override: opts.shell_override,
            settings: opts.settings,
            active_runs: Mutex::new(HashMap::new()),
            active_workspaces: Mutex::new(HashSet::new()),
            activity: Mutex::new((Vec::new(), HashMap::new())),
        })
    }

    /// Live-activity snapshot of a run (empty when unknown/evicted).
    pub fn run_activity(&self, run_id: &str) -> Vec<ActivityEntry> {
        self.activity
            .lock()
            .unwrap()
            .1
            .get(run_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Append one activity line; consecutive think/text deltas from the same
    /// step merge into the previous entry so the feed stays readable.
    fn push_activity(&self, run_id: &str, step_id: &str, kind: &str, text: &str) {
        let mut guard = self.activity.lock().unwrap();
        let (order, map) = &mut *guard;
        if !map.contains_key(run_id) {
            order.push(run_id.to_string());
            while order.len() > ACTIVITY_MAX_RUNS {
                let oldest = order.remove(0);
                map.remove(&oldest);
            }
            map.insert(run_id.to_string(), Vec::new());
        }
        let entries = map.get_mut(run_id).unwrap();
        let mergeable = matches!(kind, "think" | "text");
        if mergeable {
            if let Some(last) = entries.last_mut() {
                if last.kind == kind && last.step_id == step_id {
                    if last.text.len() < ACTIVITY_MAX_TEXT {
                        last.text.push_str(text);
                        if last.text.len() > ACTIVITY_MAX_TEXT {
                            last.text.truncate(ACTIVITY_MAX_TEXT);
                        }
                    }
                    return;
                }
            }
        }
        let mut t = text.to_string();
        if t.len() > ACTIVITY_MAX_TEXT {
            t.truncate(ACTIVITY_MAX_TEXT);
        }
        entries.push(ActivityEntry {
            ts: now_iso(),
            step_id: step_id.to_string(),
            kind: kind.to_string(),
            text: t,
        });
        if entries.len() > ACTIVITY_MAX_ENTRIES {
            let drop = entries.len() - ACTIVITY_MAX_ENTRIES;
            entries.drain(0..drop);
        }
    }

    /// Await to completion (CLI).
    pub async fn run(
        self: &Arc<Self>,
        def: &WorkflowDef,
        provided: HashMap<String, String>,
        trigger: Option<String>,
    ) -> Result<WorkflowRun> {
        let (_, handle) = self.start(def, provided, trigger)?;
        Ok(handle.await?)
    }

    /// Start a run: returns the initial run snapshot (with id) plus a join
    /// handle resolving to the final run. Errors synchronously on missing
    /// required inputs or a busy workspace.
    pub fn start(
        self: &Arc<Self>,
        def: &WorkflowDef,
        provided: HashMap<String, String>,
        trigger: Option<String>,
    ) -> Result<(WorkflowRun, tokio::task::JoinHandle<WorkflowRun>)> {
        let inputs = resolve_inputs(def, provided)?;
        let workspace = self.resolve_workspace(def);
        let workspace_key = workspace.to_string_lossy().to_string();
        {
            // The workspace is a shared persistent dir — concurrent runs
            // would trample each other.
            let mut ws = self.active_workspaces.lock().unwrap();
            if !ws.insert(workspace_key.clone()) {
                bail!(
                    "a run is already in progress for this workspace ({})",
                    workspace.display()
                );
            }
        }

        let mut run = match self
            .store
            .new_run(&def.name, inputs.clone(), &workspace, trigger)
        {
            Ok(r) => r,
            Err(e) => {
                self.active_workspaces
                    .lock()
                    .unwrap()
                    .remove(&workspace_key);
                return Err(e);
            }
        };
        run.steps = def
            .steps
            .iter()
            .map(|s| StepRun {
                id: s.id.clone(),
                kind: s.kind,
                persona: s.persona.clone(),
                depends_on: s.depends_on.clone(),
                status: StepStatus::Pending,
                result: String::new(),
                error: None,
                observe: None,
                guidance_snapshot: None,
                started_at: None,
                completed_at: None,
            })
            .collect();
        self.emit(&run);

        let control = Arc::new(RunControl {
            cancel: CancellationToken::new(),
        });
        self.active_runs
            .lock()
            .unwrap()
            .insert(run.id.clone(), Arc::clone(&control));

        let snapshot = run.clone();
        let this = Arc::clone(self);
        let def = def.clone();
        let handle = tokio::spawn(async move { this.execute(run, def, inputs, control).await });
        Ok((snapshot, handle))
    }

    /// Cancel: stop dispatching new steps, mark cancelled, abort in-flight
    /// steps. Returns false if the run isn't active.
    pub fn cancel(&self, run_id: &str) -> bool {
        match self.active_runs.lock().unwrap().get(run_id) {
            Some(control) => {
                control.cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub fn is_running(&self, run_id: &str) -> bool {
        self.active_runs.lock().unwrap().contains_key(run_id)
    }

    // ===== Internal =====

    async fn execute(
        self: Arc<Self>,
        mut run: WorkflowRun,
        def: WorkflowDef,
        inputs: HashMap<String, String>,
        control: Arc<RunControl>,
    ) -> WorkflowRun {
        let observe_dir = Path::new(&run.run_dir)
            .join(".observe")
            .to_string_lossy()
            .to_string();
        let step_defs: HashMap<String, usize> = def
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.clone(), i))
            .collect();
        let mut step_results: HashMap<String, String> = HashMap::new();
        let mut join_set: JoinSet<(String, StepRunResult)> = JoinSet::new();
        // Agent (LLM) steps in flight. Many providers reject concurrent
        // requests, so agent steps beyond the `llm_parallel` budget are NOT
        // dispatched — they stay `pending` (their timeout only starts when
        // they actually run). Script steps are unaffected.
        let mut active_agents: usize = 0;

        loop {
            // 1. Upstream failed/skipped → cascade skip.
            let now = now_iso();
            let statuses: HashMap<String, StepStatus> =
                run.steps.iter().map(|s| (s.id.clone(), s.status)).collect();
            let mut changed = false;
            for sr in run.steps.iter_mut() {
                if sr.status == StepStatus::Pending
                    && sr.depends_on.iter().any(|d| {
                        matches!(
                            statuses.get(d),
                            Some(StepStatus::Failed) | Some(StepStatus::Skipped)
                        )
                    })
                {
                    sr.status = StepStatus::Skipped;
                    sr.completed_at = Some(now.clone());
                    changed = true;
                }
            }
            if changed {
                self.emit(&run);
                continue; // skips may cascade further
            }

            // 2. While not cancelled and under the caps, dispatch ready steps.
            while !control.cancel.is_cancelled() && join_set.len() < self.concurrency {
                let llm_parallel = self
                    .settings
                    .llm_parallel
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .max(1);
                let statuses: HashMap<String, StepStatus> =
                    run.steps.iter().map(|s| (s.id.clone(), s.status)).collect();
                let Some(sr) = run.steps.iter_mut().find(|s| {
                    s.status == StepStatus::Pending
                        && (s.kind != StepKind::Agent || active_agents < llm_parallel)
                        && s.depends_on
                            .iter()
                            .all(|d| statuses.get(d) == Some(&StepStatus::Done))
                }) else {
                    break;
                };
                if sr.kind == StepKind::Agent {
                    active_agents += 1;
                }

                sr.status = StepStatus::Running;
                sr.started_at = Some(now_iso());
                let step_id = sr.id.clone();
                self.emit(&run);

                let step_def = def.steps[step_defs[&step_id]].clone();
                let ctx = RenderContext {
                    inputs: inputs.clone(),
                    step_results: step_results.clone(),
                    run_dir: run.run_dir.clone(),
                };
                let this = Arc::clone(&self);
                let def = def.clone();
                let observe_dir = observe_dir.clone();
                let cancel = control.cancel.clone();
                // Live-activity sink: the isolated agent streams thinking /
                // tool calls / messages here; the UI polls them per run.
                let activity = {
                    let sink = Arc::clone(&self);
                    let rid = run.id.clone();
                    let sid = step_id.clone();
                    crate::agent::isolated_runner::OnActivity(Arc::new(move |kind, text| {
                        sink.push_activity(&rid, &sid, kind, text)
                    }))
                };
                join_set.spawn(async move {
                    let res = this
                        .execute_step(&step_def, &def, &ctx, &observe_dir, activity, cancel)
                        .await;
                    (step_id, res)
                });
            }

            // 3. Terminate: nothing in flight and (all terminal or cancelled).
            if join_set.is_empty() {
                let all_terminal = run.steps.iter().all(|s| s.status.is_terminal());
                if all_terminal || control.cancel.is_cancelled() {
                    break;
                }
                // Nothing running and nothing ready — should be unreachable
                // for a validated DAG; bail out instead of spinning.
                tracing::warn!(
                    "[WorkflowExecutor] run {} stalled with no runnable steps",
                    run.id
                );
                break;
            }

            // 4. Wait for the next step to finish.
            if let Some(Ok((step_id, res))) = join_set.join_next().await {
                if def.steps[step_defs[&step_id]].kind == StepKind::Agent {
                    active_agents = active_agents.saturating_sub(1);
                }
                if let Some(sr) = run.steps.iter_mut().find(|s| s.id == step_id) {
                    apply_result(
                        sr,
                        def.steps[step_defs[&step_id]].observe.as_ref(),
                        res,
                        &run.run_dir,
                    );
                    if sr.status == StepStatus::Done {
                        step_results.insert(step_id, sr.result.clone());
                    }
                }
                self.emit(&run);
            }
        }

        // Finalize.
        if control.cancel.is_cancelled() {
            let now = now_iso();
            for sr in run.steps.iter_mut() {
                if sr.status == StepStatus::Pending {
                    sr.status = StepStatus::Skipped;
                    sr.completed_at = Some(now.clone());
                }
            }
            run.status = RunStatus::Cancelled;
        } else {
            let any_bad = run
                .steps
                .iter()
                .any(|s| matches!(s.status, StepStatus::Failed | StepStatus::Skipped));
            run.status = if any_bad {
                RunStatus::PartialFailed
            } else {
                RunStatus::Done
            };
        }
        run.completed_at = Some(now_iso());
        self.active_runs.lock().unwrap().remove(&run.id);
        self.active_workspaces.lock().unwrap().remove(&run.run_dir);
        self.emit(&run);
        run
    }

    async fn execute_step(
        &self,
        step: &super::types::WorkflowStep,
        def: &WorkflowDef,
        ctx: &RenderContext,
        observe_dir: &str,
        activity: crate::agent::isolated_runner::OnActivity,
        cancel: CancellationToken,
    ) -> StepRunResult {
        match step.kind {
            StepKind::Agent => {
                let Some(persona_name) = step.persona.as_deref() else {
                    return StepRunResult {
                        failed: true,
                        error: Some(format!("agent step \"{}\" missing persona", step.id)),
                        ..Default::default()
                    };
                };
                let persona: Option<PersonaConfig> = self
                    .persona_registry
                    .lock()
                    .unwrap()
                    .get(persona_name)
                    .cloned();
                let Some(persona) = persona else {
                    return StepRunResult {
                        failed: true,
                        error: Some(format!("persona \"{persona_name}\" not found")),
                        ..Default::default()
                    };
                };
                let mut skills_dirs = self.skills_extra_dirs.clone();
                let ws_skills = Path::new(&ctx.run_dir).join("skills");
                if ws_skills.is_dir() {
                    skills_dirs.push(ws_skills.to_string_lossy().to_string());
                }
                // Guarantee-result loop: transient LLM failures (session
                // error / empty reply) get up to `agent_retries` extra
                // attempts before the step is marked failed.
                let retries = self
                    .settings
                    .agent_retries
                    .load(std::sync::atomic::Ordering::Relaxed);
                let mut attempt = 0usize;
                loop {
                    let res = run_agent_step(
                        step,
                        &persona,
                        def,
                        ctx,
                        skills_dirs.clone(),
                        &self.extra_mcp_servers,
                        Some(activity.clone()),
                        cancel.clone(),
                    )
                    .await;
                    if !(res.failed && res.retryable) || attempt >= retries || cancel.is_cancelled()
                    {
                        break res;
                    }
                    attempt += 1;
                    (activity.0)(
                        "status",
                        &format!(
                            "attempt {attempt} failed ({}), retrying…",
                            res.error.as_deref().unwrap_or("?")
                        ),
                    );
                    tracing::warn!(
                        "[WorkflowExecutor] step \"{}\" attempt {attempt}/{} failed ({}), retrying",
                        step.id,
                        retries + 1,
                        res.error.as_deref().unwrap_or("?")
                    );
                }
            }
            StepKind::Script => {
                // The workspace IS the persistent dir: WF_RUN_DIR ==
                // WF_WORKFLOW_DIR == ctx.run_dir.
                run_script_step(
                    step,
                    def,
                    ctx,
                    observe_dir,
                    Some(&ctx.run_dir),
                    self.shell_override.as_deref(),
                    cancel,
                )
                .await
            }
        }
    }

    /// Resolve the workflow's workspace dir (every step's cwd, persistent
    /// across runs):
    ///   - no `workspace` → `<workflow_data_dir>/<sanitized-name>/` (default)
    ///   - `~`-prefixed → expanded to home
    ///   - absolute → as-is; relative → under the default root
    fn resolve_workspace(&self, def: &WorkflowDef) -> PathBuf {
        let custom = def.workspace.as_deref().map(str::trim).unwrap_or("");
        if custom.is_empty() {
            return self.workflow_data_dir.join(sanitize_name(&def.name));
        }
        let expanded = if let Some(rest) = custom.strip_prefix("~/") {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(rest)
        } else if custom == "~" {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
        } else {
            PathBuf::from(custom)
        };
        if expanded.is_absolute() {
            expanded
        } else {
            self.workflow_data_dir.join(expanded)
        }
    }

    fn emit(&self, run: &WorkflowRun) {
        self.store.persist(run);
        if let Some(cb) = &self.on_update {
            cb(run);
        }
    }
}

fn resolve_inputs(
    def: &WorkflowDef,
    provided: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let mut inputs = provided;
    for inp in &def.inputs {
        if !inputs.contains_key(&inp.name) {
            if let Some(d) = &inp.default {
                inputs.insert(inp.name.clone(), d.clone());
            } else if inp.required {
                bail!(
                    "workflow \"{}\" missing required input \"{}\"",
                    def.name,
                    inp.name
                );
            }
        }
    }
    Ok(inputs)
}

fn apply_result(
    sr: &mut StepRun,
    observe_spec: Option<&ObserveSpec>,
    res: StepRunResult,
    run_dir: &str,
) {
    // Steps aborted by cancel: recorded as skipped (their output is invalid
    // and doesn't enter step_results), consistent with cancel cascading.
    if res.aborted {
        sr.status = StepStatus::Skipped;
        sr.error = res.error;
        sr.completed_at = Some(now_iso());
        return;
    }
    sr.result = res.result;
    sr.error = res.error;
    sr.guidance_snapshot = res.guidance_snapshot;
    sr.status = if res.failed {
        StepStatus::Failed
    } else {
        StepStatus::Done
    };
    sr.completed_at = Some(now_iso());
    if sr.status == StepStatus::Done {
        if let Some(spec) = observe_spec {
            sr.observe = capture_observe(spec, &sr.result, run_dir);
        }
    }
}

/// Capture observe output; a missing source returns `None` (observe is an
/// optional side channel and must never block the run).
fn capture_observe(spec: &ObserveSpec, result: &str, run_dir: &str) -> Option<ObserveOutput> {
    match &spec.from {
        ObserveFrom::Result => Some(ObserveOutput {
            label: spec.label.clone(),
            r#as: spec.r#as,
            content: Some(result.to_string()),
            artifact_path: None,
        }),
        ObserveFrom::File(file) => {
            let abs = if Path::new(file).is_absolute() {
                PathBuf::from(file)
            } else {
                Path::new(run_dir).join(file)
            };
            match spec.r#as {
                ObserveAs::Artifact => abs.exists().then(|| ObserveOutput {
                    label: spec.label.clone(),
                    r#as: spec.r#as,
                    content: None,
                    artifact_path: Some(abs.to_string_lossy().to_string()),
                }),
                ObserveAs::Inline => {
                    std::fs::read_to_string(&abs)
                        .ok()
                        .map(|content| ObserveOutput {
                            label: spec.label.clone(),
                            r#as: spec.r#as,
                            content: Some(content),
                            artifact_path: None,
                        })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::registry::parse_workflow_file;
    use crate::workflow::run_store::WorkflowRunStore;

    fn write_wf(dir: &Path, name: &str, body: &str) -> WorkflowDef {
        let path = dir.join(format!("{name}.md"));
        std::fs::write(&path, body).unwrap();
        parse_workflow_file(&path).unwrap()
    }

    fn executor(dir: &Path) -> Arc<WorkflowExecutor> {
        let store = Arc::new(WorkflowRunStore::new(dir.join("runs.json")));
        let registry = Arc::new(Mutex::new(PersonaRegistry::new(dir.join("personas"))));
        WorkflowExecutor::new(WorkflowExecutorOpts {
            store,
            persona_registry: registry,
            concurrency: None,
            on_update: None,
            workflow_data_dir: dir.join("wf-data"),
            skills_extra_dirs: vec![],
            extra_mcp_servers: vec![],
            shell_override: None,
            settings: LiveWorkflowSettings::default(),
        })
    }

    #[tokio::test]
    async fn script_chain_passes_results_downstream() {
        let dir = tempfile::tempdir().unwrap();
        let def = write_wf(
            dir.path(),
            "chain",
            r#"---
inputs:
  - { name: who, required: true }
steps:
  - id: first
    kind: script
    run: echo "hello $WF_INPUT_WHO"
  - id: second
    kind: script
    run: echo "got[$WF_STEP_FIRST_RESULT]"
---
"#,
        );
        let ex = executor(dir.path());
        let run = ex
            .run(
                &def,
                HashMap::from([("who".to_string(), "wf".to_string())]),
                Some("test".into()),
            )
            .await
            .unwrap();
        assert_eq!(run.status, RunStatus::Done);
        assert_eq!(run.steps[0].result, "hello wf");
        assert_eq!(run.steps[1].result, "got[hello wf]");
        // Default workspace under workflow_data_dir/<name>.
        assert!(run.run_dir.ends_with("chain"));
    }

    #[tokio::test]
    async fn failure_cascades_and_marks_partial_failed() {
        let dir = tempfile::tempdir().unwrap();
        let def = write_wf(
            dir.path(),
            "cascade",
            r#"---
steps:
  - { id: ok, kind: script, run: echo fine }
  - { id: boom, kind: script, run: "exit 7" }
  - { id: after, kind: script, run: echo x, dependsOn: [boom] }
  - { id: last, kind: script, run: echo y, dependsOn: [after, ok] }
---
"#,
        );
        let ex = executor(dir.path());
        let run = ex.run(&def, HashMap::new(), None).await.unwrap();
        assert_eq!(run.status, RunStatus::PartialFailed);
        let by_id: HashMap<_, _> = run.steps.iter().map(|s| (s.id.as_str(), s)).collect();
        assert_eq!(by_id["ok"].status, StepStatus::Done);
        assert_eq!(by_id["boom"].status, StepStatus::Failed);
        assert_eq!(by_id["after"].status, StepStatus::Skipped);
        assert_eq!(by_id["last"].status, StepStatus::Skipped);
    }

    #[tokio::test]
    async fn missing_required_input_fails_sync() {
        let dir = tempfile::tempdir().unwrap();
        let def = write_wf(
            dir.path(),
            "needs-input",
            "---\ninputs:\n  - { name: x, required: true }\nsteps:\n  - { id: a, kind: script, run: echo 1 }\n---\n",
        );
        let ex = executor(dir.path());
        let err = ex.start(&def, HashMap::new(), None).unwrap_err();
        assert!(err.to_string().contains("missing required input"));
        // Workspace slot must be released after the failed start.
        assert!(ex
            .start(&def, HashMap::from([("x".into(), "1".into())]), None)
            .is_ok());
    }

    #[tokio::test]
    async fn default_input_applies() {
        let dir = tempfile::tempdir().unwrap();
        let def = write_wf(
            dir.path(),
            "defaults",
            "---\ninputs:\n  - { name: d, default: dv }\nsteps:\n  - { id: a, kind: script, run: \"echo $WF_INPUT_D\" }\n---\n",
        );
        let ex = executor(dir.path());
        let run = ex.run(&def, HashMap::new(), None).await.unwrap();
        assert_eq!(run.steps[0].result, "dv");
    }

    #[tokio::test]
    async fn concurrent_same_workspace_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let def = write_wf(
            dir.path(),
            "slow",
            "---\nsteps:\n  - { id: a, kind: script, run: sleep 5 }\n---\n",
        );
        let ex = executor(dir.path());
        let (_r1, h1) = ex.start(&def, HashMap::new(), None).unwrap();
        let err = ex.start(&def, HashMap::new(), None).unwrap_err();
        assert!(err.to_string().contains("already in progress"), "{err}");
        h1.abort();
        // Manual cleanup since we aborted the driver task mid-flight.
        ex.active_workspaces.lock().unwrap().clear();
    }

    #[tokio::test]
    async fn cancel_skips_pending_and_aborts_running() {
        let dir = tempfile::tempdir().unwrap();
        let def = write_wf(
            dir.path(),
            "cancelme",
            r#"---
steps:
  - { id: slow, kind: script, run: sleep 30 }
  - { id: after, kind: script, run: echo done, dependsOn: [slow] }
---
"#,
        );
        let ex = executor(dir.path());
        let (snapshot, handle) = ex.start(&def, HashMap::new(), None).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(ex.cancel(&snapshot.id));
        let run = handle.await.unwrap();
        assert_eq!(run.status, RunStatus::Cancelled);
        let by_id: HashMap<_, _> = run.steps.iter().map(|s| (s.id.as_str(), s)).collect();
        assert_eq!(by_id["slow"].status, StepStatus::Skipped); // aborted → skipped
        assert_eq!(by_id["after"].status, StepStatus::Skipped);
        assert!(!ex.is_running(&snapshot.id));
    }

    #[tokio::test]
    async fn observe_inline_and_artifact_capture() {
        let dir = tempfile::tempdir().unwrap();
        let def = write_wf(
            dir.path(),
            "observer",
            r#"---
steps:
  - id: a
    kind: script
    run: |
      echo "artifact body" > out.txt
      echo summary
    observe: { label: "sum", from: result, as: inline }
  - id: b
    kind: script
    run: echo ok
    dependsOn: [a]
    observe: { label: "file", from: { file: out.txt }, as: artifact }
---
"#,
        );
        let ex = executor(dir.path());
        let run = ex.run(&def, HashMap::new(), None).await.unwrap();
        assert_eq!(run.status, RunStatus::Done);
        let obs_a = run.steps[0].observe.as_ref().unwrap();
        assert_eq!(obs_a.content.as_deref(), Some("summary"));
        let obs_b = run.steps[1].observe.as_ref().unwrap();
        assert!(obs_b.artifact_path.as_deref().unwrap().ends_with("out.txt"));
    }

    #[tokio::test]
    async fn agent_step_with_unknown_persona_fails() {
        let dir = tempfile::tempdir().unwrap();
        let def = write_wf(
            dir.path(),
            "agents",
            "---\nsteps:\n  - { id: a, kind: agent, persona: ghost, prompt: hi }\n---\n",
        );
        let ex = executor(dir.path());
        let run = ex.run(&def, HashMap::new(), None).await.unwrap();
        assert_eq!(run.status, RunStatus::PartialFailed);
        assert_eq!(run.steps[0].status, StepStatus::Failed);
        assert!(run.steps[0].error.as_deref().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn parallel_fanout_respects_dag() {
        let dir = tempfile::tempdir().unwrap();
        let def = write_wf(
            dir.path(),
            "fanout",
            r#"---
steps:
  - { id: n1, kind: script, run: "sleep 0.3; echo 1" }
  - { id: n2, kind: script, run: "sleep 0.3; echo 2" }
  - { id: n3, kind: script, run: "sleep 0.3; echo 3" }
  - id: agg
    kind: script
    run: echo "$WF_STEP_N1_RESULT+$WF_STEP_N2_RESULT+$WF_STEP_N3_RESULT"
---
"#,
        );
        let ex = executor(dir.path());
        let started = std::time::Instant::now();
        let run = ex.run(&def, HashMap::new(), None).await.unwrap();
        assert_eq!(run.status, RunStatus::Done);
        assert_eq!(run.steps[3].result, "1+2+3");
        // Three 300ms siblings in parallel + agg ≪ 4×300ms serial.
        assert!(started.elapsed() < std::time::Duration::from_millis(1100));
    }
}

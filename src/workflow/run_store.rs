//! WorkflowRunStore — run state persistence.
//!
//! Port of `SemaClaw/src/workflow/runStore.ts`.
//!
//!   - Memory is authoritative: read `workflow-runs.json` once at
//!     construction; all reads/writes go through memory afterwards.
//!   - Flush policy: intermediate (running) states are throttled
//!     ([`FLUSH_THROTTLE`]); terminal states write immediately so completed
//!     runs are never lost. tmp+rename atomic writes.
//!   - Single writer + memory authority → no load-modify-write lost updates
//!     between concurrent runs.
//!   - id = `<workflow-name>-<NNNN>`, NNNN monotonically increasing per
//!     workflow (seeded from disk, never reused after eviction).
//!   - Retention: the most recent [`PER_WORKFLOW_CAP`] run records per
//!     workflow; older records are dropped (workspace files are untouched —
//!     the workspace is a persistent per-workflow dir owned by the user).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::types::{now_iso, sanitize_name, RunStatus, StepStatus, WorkflowRun};

/// Run records retained per workflow (oldest dropped beyond this).
const PER_WORKFLOW_CAP: usize = 10;
/// Minimum interval between disk writes for intermediate (running) states.
const FLUSH_THROTTLE: Duration = Duration::from_millis(400);

struct Inner {
    /// Authoritative state, newest first.
    runs: Vec<WorkflowRun>,
    /// id prefix → highest allocated sequence number (monotonic).
    seq_by_prefix: HashMap<String, u64>,
    last_write: Option<Instant>,
    dirty: bool,
}

pub struct WorkflowRunStore {
    state_path: PathBuf,
    per_workflow_cap: usize,
    inner: Mutex<Inner>,
}

impl WorkflowRunStore {
    pub fn new(state_path: PathBuf) -> Self {
        Self::with_cap(state_path, PER_WORKFLOW_CAP)
    }

    pub fn with_cap(state_path: PathBuf, per_workflow_cap: usize) -> Self {
        let runs = read_from_disk(&state_path);
        Self {
            state_path,
            per_workflow_cap,
            inner: Mutex::new(Inner {
                runs,
                seq_by_prefix: HashMap::new(),
                last_write: None,
                dirty: false,
            }),
        }
    }

    /// Create a new run (allocates an id), status `running`, empty steps —
    /// not yet persisted. `workspace_dir` is resolved by the caller and
    /// shared across runs; this ensures it and its `.observe` subdir exist.
    pub fn new_run(
        &self,
        workflow_name: &str,
        inputs: HashMap<String, String>,
        workspace_dir: &Path,
        trigger: Option<String>,
    ) -> Result<WorkflowRun> {
        let id = self.allocate_id(workflow_name);
        std::fs::create_dir_all(workspace_dir.join(".observe"))?;
        Ok(WorkflowRun {
            id,
            workflow_name: workflow_name.to_string(),
            label: None,
            inputs,
            status: RunStatus::Running,
            run_dir: workspace_dir.to_string_lossy().to_string(),
            steps: Vec::new(),
            trigger,
            created_at: now_iso(),
            completed_at: None,
        })
    }

    /// Upsert a run (by id) into memory; terminal states flush immediately,
    /// running states are throttled.
    pub fn persist(&self, run: &WorkflowRun) {
        let mut inner = self.inner.lock().unwrap();
        match inner.runs.iter().position(|r| r.id == run.id) {
            Some(idx) => {
                let mut incoming = run.clone();
                // The executor's working copy predates any user rename —
                // don't let a mid-run emit wipe the label.
                if incoming.label.is_none() {
                    incoming.label = inner.runs[idx].label.clone();
                }
                inner.runs[idx] = incoming;
            }
            None => inner.runs.insert(0, run.clone()),
        }
        enforce_cap(&mut inner.runs, &run.workflow_name, self.per_workflow_cap);

        if run.status == RunStatus::Running {
            let due = inner
                .last_write
                .map_or(true, |t| t.elapsed() >= FLUSH_THROTTLE);
            if due {
                self.write_now(&mut inner);
            } else {
                inner.dirty = true;
            }
        } else {
            self.write_now(&mut inner); // terminal: never lose a completed run
        }
    }

    /// All runs (newest first), cloned snapshot.
    pub fn load(&self) -> Vec<WorkflowRun> {
        self.inner.lock().unwrap().runs.clone()
    }

    pub fn get(&self, id: &str) -> Option<WorkflowRun> {
        self.inner
            .lock()
            .unwrap()
            .runs
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    /// Startup reconciliation: runs still marked `running` are orphans — the
    /// process restarted and their scheduling loop is gone, so they will
    /// never progress. Mark them `interrupted` and settle their steps
    /// (running→failed, pending→skipped). Call only at startup (no active
    /// runs). Returns the number of reconciled runs.
    pub fn reconcile_orphans(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let ts = now_iso();
        let mut n = 0;
        for run in inner.runs.iter_mut() {
            if run.status != RunStatus::Running {
                continue;
            }
            n += 1;
            run.status = RunStatus::Interrupted;
            run.completed_at.get_or_insert_with(|| ts.clone());
            for s in run.steps.iter_mut() {
                match s.status {
                    StepStatus::Running => {
                        s.status = StepStatus::Failed;
                        s.error
                            .get_or_insert_with(|| "interrupted: daemon restarted".to_string());
                        s.completed_at.get_or_insert_with(|| ts.clone());
                    }
                    StepStatus::Pending => {
                        s.status = StepStatus::Skipped;
                        s.completed_at.get_or_insert_with(|| ts.clone());
                    }
                    _ => {}
                }
            }
        }
        if n > 0 {
            self.write_now(&mut inner);
        }
        n
    }

    /// Rename a run (user display label; empty clears it back to the id).
    pub fn rename_run(&self, id: &str, label: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let Some(run) = inner.runs.iter_mut().find(|r| r.id == id) else {
            return Err(format!("run \"{id}\" not found"));
        };
        run.label = {
            let t = label.trim();
            (!t.is_empty()).then(|| t.to_string())
        };
        self.write_now(&mut inner);
        Ok(())
    }

    /// Delete a run record (history only — workspace files are untouched).
    pub fn delete_run(&self, id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.runs.len();
        inner.runs.retain(|r| r.id != id);
        if inner.runs.len() == before {
            return Err(format!("run \"{id}\" not found"));
        }
        self.write_now(&mut inner);
        Ok(())
    }

    /// Flush any pending state to disk immediately (shutdown path).
    pub fn flush(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.dirty || inner.last_write.is_none() {
            self.write_now(&mut inner);
        }
    }

    // ===== Internal =====

    /// id = `<sanitized name>-<NNNN>`; the in-memory counter is seeded lazily
    /// from on-disk runs (max), then only increments — evicting the oldest
    /// run never causes the sequence to regress or collide.
    fn allocate_id(&self, workflow_name: &str) -> String {
        let prefix = format!("{}-", sanitize_name(workflow_name));
        let mut inner = self.inner.lock().unwrap();
        let last = match inner.seq_by_prefix.get(&prefix) {
            Some(&n) => n,
            None => inner
                .runs
                .iter()
                .filter_map(|r| r.id.strip_prefix(&prefix))
                .filter_map(|rest| rest.parse::<u64>().ok())
                .max()
                .unwrap_or(0),
        };
        let next = last + 1;
        inner.seq_by_prefix.insert(prefix.clone(), next);
        format!("{prefix}{next:04}")
    }

    fn write_now(&self, inner: &mut Inner) {
        if let Some(dir) = self.state_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let json = match serde_json::to_string_pretty(&inner.runs) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("[WorkflowRunStore] serialize failed: {e}");
                return;
            }
        };
        let tmp = self.state_path.with_extension("json.tmp");
        if let Err(e) =
            std::fs::write(&tmp, &json).and_then(|_| std::fs::rename(&tmp, &self.state_path))
        {
            tracing::warn!("[WorkflowRunStore] write failed: {e}");
            return;
        }
        inner.last_write = Some(Instant::now());
        inner.dirty = false;
    }
}

/// Trim a workflow's history to the newest `cap` records (records only —
/// never touches workspace directories).
fn enforce_cap(runs: &mut Vec<WorkflowRun>, workflow_name: &str, cap: usize) {
    let mut kept = 0;
    runs.retain(|r| {
        if r.workflow_name != workflow_name {
            return true;
        }
        kept += 1;
        kept <= cap
    });
}

fn read_from_disk(path: &Path) -> Vec<WorkflowRun> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> WorkflowRunStore {
        WorkflowRunStore::new(dir.join("workflow-runs.json"))
    }

    #[test]
    fn new_run_allocates_sequential_ids_and_creates_observe_dir() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(dir.path());
        let ws = dir.path().join("ws");
        let r1 = st
            .new_run("my wf", HashMap::new(), &ws, Some("cli".into()))
            .unwrap();
        let r2 = st.new_run("my wf", HashMap::new(), &ws, None).unwrap();
        assert_eq!(r1.id, "my_wf-0001");
        assert_eq!(r2.id, "my_wf-0002");
        assert!(ws.join(".observe").is_dir());
    }

    #[test]
    fn persist_terminal_writes_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(dir.path());
        let ws = dir.path().join("ws");
        let mut run = st.new_run("wf", HashMap::new(), &ws, None).unwrap();
        run.status = RunStatus::Done;
        st.persist(&run);

        // Fresh store re-reads from disk; sequence continues after the max.
        let st2 = store(dir.path());
        assert_eq!(st2.load().len(), 1);
        assert_eq!(st2.get("wf-0001").unwrap().status, RunStatus::Done);
        let r2 = st2.new_run("wf", HashMap::new(), &ws, None).unwrap();
        assert_eq!(r2.id, "wf-0002");
    }

    #[test]
    fn cap_drops_oldest_records() {
        let dir = tempfile::tempdir().unwrap();
        let st = WorkflowRunStore::with_cap(dir.path().join("s.json"), 3);
        let ws = dir.path().join("ws");
        for _ in 0..5 {
            let mut run = st.new_run("wf", HashMap::new(), &ws, None).unwrap();
            run.status = RunStatus::Done;
            st.persist(&run);
        }
        let runs = st.load();
        assert_eq!(runs.len(), 3);
        // Newest first, oldest ids evicted.
        assert_eq!(runs[0].id, "wf-0005");
        assert_eq!(runs[2].id, "wf-0003");
        // Sequence never regresses after eviction.
        let r = st.new_run("wf", HashMap::new(), &ws, None).unwrap();
        assert_eq!(r.id, "wf-0006");
    }

    #[test]
    fn rename_and_delete_run() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(dir.path());
        let ws = dir.path().join("ws");
        let mut run = st.new_run("wf", HashMap::new(), &ws, None).unwrap();
        run.status = RunStatus::Done;
        st.persist(&run);

        st.rename_run("wf-0001", "Nghiên cứu tuần 27").unwrap();
        assert_eq!(
            st.get("wf-0001").unwrap().label.as_deref(),
            Some("Nghiên cứu tuần 27")
        );
        // A later executor-style persist (label: None) must NOT wipe it.
        st.persist(&run);
        assert_eq!(
            st.get("wf-0001").unwrap().label.as_deref(),
            Some("Nghiên cứu tuần 27")
        );
        // Empty label clears.
        st.rename_run("wf-0001", "  ").unwrap();
        assert!(st.get("wf-0001").unwrap().label.is_none());

        st.delete_run("wf-0001").unwrap();
        assert!(st.get("wf-0001").is_none());
        assert!(st.delete_run("wf-0001").is_err());
        assert!(st.rename_run("wf-0001", "x").is_err());
    }

    #[test]
    fn reconcile_orphans_settles_running_runs() {
        let dir = tempfile::tempdir().unwrap();
        let st = store(dir.path());
        let ws = dir.path().join("ws");
        let mut run = st.new_run("wf", HashMap::new(), &ws, None).unwrap();
        run.steps = vec![
            crate::workflow::types::StepRun {
                id: "a".into(),
                kind: crate::workflow::types::StepKind::Script,
                persona: None,
                depends_on: vec![],
                status: StepStatus::Running,
                result: String::new(),
                error: None,
                observe: None,
                guidance_snapshot: None,
                started_at: None,
                completed_at: None,
            },
            crate::workflow::types::StepRun {
                id: "b".into(),
                kind: crate::workflow::types::StepKind::Script,
                persona: None,
                depends_on: vec!["a".into()],
                status: StepStatus::Pending,
                result: String::new(),
                error: None,
                observe: None,
                guidance_snapshot: None,
                started_at: None,
                completed_at: None,
            },
        ];
        st.persist(&run);

        let st2 = store(dir.path());
        assert_eq!(st2.reconcile_orphans(), 1);
        let r = st2.get(&run.id).unwrap();
        assert_eq!(r.status, RunStatus::Interrupted);
        assert_eq!(r.steps[0].status, StepStatus::Failed);
        assert_eq!(r.steps[1].status, StepStatus::Skipped);
        // Idempotent.
        assert_eq!(st2.reconcile_orphans(), 0);
    }
}

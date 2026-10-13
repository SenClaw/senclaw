//! Running datagen jobs: start, watch, cancel.
//!
//! A job is a tokio task writing one dataset folder. Its live status sits in
//! a process-wide registry (a field on `UiState` would mean touching every
//! place that builds one); the folder's manifest is the durable record. A
//! manifest left at `running` with no live job is reported as `interrupted`:
//! the daemon stopped mid-run, and the records written so far are still there.
//!
//! A job started from a chat carries that chat as its `owner`, and the chat's
//! tools only ever see, list and cancel its own jobs and datasets — the same
//! rule `watch_stop` follows. The owner comes from the MCP server's own
//! environment, never from a tool parameter.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::generator::AcceptLoop;
use super::llm::Llm;
use super::store::{self, DatasetWriter};
use super::executable_qa::CodeRunner;
use super::{doc_qa, executable_qa, expand_seeds, export_logs, reasoning_traces};

/// Jobs allowed to run at once. Each one is a stream of LLM calls on the
/// user's account; a third is almost always a mistake.
pub const MAX_RUNNING: usize = 2;
/// Finished jobs kept in memory for `status`; older ones live on as manifests.
const KEEP_FINISHED: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "pipeline", rename_all = "snake_case")]
pub enum Pipeline {
    ExpandSeeds(expand_seeds::Params),
    ExportLogs(export_logs::Params),
    DocQa(doc_qa::Params),
    ReasoningTraces(reasoning_traces::Params),
    ExecutableQa(executable_qa::Params),
}

impl Pipeline {
    pub fn name(&self) -> &'static str {
        match self {
            Pipeline::ExpandSeeds(_) => "expand_seeds",
            Pipeline::ExportLogs(_) => "export_logs",
            Pipeline::DocQa(_) => "doc_qa",
            Pipeline::ReasoningTraces(_) => "reasoning_traces",
            Pipeline::ExecutableQa(_) => "executable_qa",
        }
    }

    /// The LLM profile the pipeline asked for, when it calls a model at all.
    pub fn profile(&self) -> Option<&str> {
        match self {
            Pipeline::ExpandSeeds(p) => p.profile.as_deref(),
            Pipeline::DocQa(p) => p.profile.as_deref(),
            Pipeline::ReasoningTraces(p) => p.profile.as_deref(),
            Pipeline::ExecutableQa(p) => p.profile.as_deref(),
            Pipeline::ExportLogs(_) => None,
        }
        .map(str::trim)
        .filter(|v| !v.is_empty())
    }

    pub fn uses_llm(&self) -> bool {
        !matches!(self, Pipeline::ExportLogs(_))
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Pipeline::ExpandSeeds(p) => p.plan().map(|_| ()),
            Pipeline::ExportLogs(p) => p.validate(),
            Pipeline::DocQa(p) => p.validate(),
            Pipeline::ReasoningTraces(p) => p.validate(),
            Pipeline::ExecutableQa(p) => p.validate(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSpec {
    /// Dataset folder name; defaults to `<pipeline>-<date>`.
    #[serde(default)]
    pub name: Option<String>,
    /// The chat that started the job, when a chat did.
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(flatten)]
    pub pipeline: Pipeline,
}

/// How a pipeline ended when it did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Finished,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    pub job_id: String,
    pub name: String,
    pub pipeline: String,
    pub owner: Option<String>,
    pub state: JobState,
    pub target: u32,
    pub accepted: u32,
    pub attempts: u32,
    pub llm_calls: u32,
    pub max_llm_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub rejected: BTreeMap<String, u64>,
    pub model: Option<String>,
    pub note: Option<String>,
    pub error: Option<String>,
    pub dir: String,
    pub started_at: u64,
    pub finished_at: Option<u64>,
}

/// Why a job did not start; each maps to its own HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// The parameters are wrong or no model is configured.
    Invalid(String),
    /// Too many jobs are running.
    Busy(String),
    /// The dataset folder could not be created.
    Io(String),
}

impl StartError {
    pub fn message(&self) -> &str {
        match self {
            StartError::Invalid(m) | StartError::Busy(m) | StartError::Io(m) => m,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelResult {
    Cancelled,
    NotRunning,
    NotFound,
}

/// What a pipeline needs from outside.
pub struct Deps {
    pub datasets_root: PathBuf,
    pub trajectories_root: PathBuf,
    /// Required by the generating pipelines; `export_logs` makes no call.
    pub llm: Option<Arc<dyn Llm>>,
    /// Where `executable_qa` runs programs.
    pub runner: Option<Arc<dyn CodeRunner>>,
}

/// The handle a running pipeline reports through.
pub struct JobCtx {
    pub job_id: String,
    pub cancel: CancellationToken,
    status: Arc<Mutex<JobStatus>>,
    writer: DatasetWriter,
}

impl JobCtx {
    pub fn write(&mut self, record: &Value) -> io::Result<()> {
        self.writer.append(record)
    }

    pub fn dir(&self) -> &Path {
        self.writer.dir()
    }

    /// Copy the loop's counters into the visible status.
    pub fn publish(&self, l: &AcceptLoop) {
        let mut s = self.status.lock().unwrap();
        s.accepted = l.accepted;
        s.attempts = l.attempts;
        s.llm_calls = l.llm_calls;
        s.input_tokens = l.input_tokens;
        s.output_tokens = l.output_tokens;
        s.rejected = l.rejected.clone();
    }

    pub fn set_target(&self, target: u32, max_llm_calls: u32) {
        let mut s = self.status.lock().unwrap();
        s.target = target;
        s.max_llm_calls = max_llm_calls;
    }

    pub fn set_model(&self, model: &str) {
        let mut s = self.status.lock().unwrap();
        if s.model.as_deref() != Some(model) {
            s.model = Some(model.to_string());
        }
    }

    pub fn set_note(&self, note: &str) {
        self.status.lock().unwrap().note = Some(note.to_string());
    }

    pub fn snapshot(&self) -> JobStatus {
        self.status.lock().unwrap().clone()
    }

    #[cfg(test)]
    pub fn for_test(root: &Path) -> JobCtx {
        let writer = DatasetWriter::create(root, "test").unwrap();
        let status = new_status("test".into(), writer.name(), "test", None, writer.dir());
        JobCtx { job_id: "test".into(), cancel: CancellationToken::new(), status: Arc::new(Mutex::new(status)), writer }
    }
}

struct Handle {
    status: Arc<Mutex<JobStatus>>,
    cancel: CancellationToken,
}

#[derive(Default)]
pub struct Registry {
    jobs: Mutex<HashMap<String, Handle>>,
}

/// The daemon's registry.
pub fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(Registry::default)
}

/// `None` sees everything (the daemon's own API); `Some(chat)` sees only
/// what that chat started.
fn visible(owner_filter: Option<&str>, owner: Option<&str>) -> bool {
    owner_filter.is_none_or(|f| owner == Some(f))
}

impl Registry {
    /// Validate, create the dataset folder, and start the job.
    pub fn start(&self, spec: JobSpec, deps: Deps) -> Result<JobStatus, StartError> {
        spec.pipeline.validate().map_err(StartError::Invalid)?;
        let llm = match (spec.pipeline.uses_llm(), deps.llm) {
            (false, _) => None,
            (true, Some(llm)) => Some(llm),
            (true, None) => return Err(StartError::Invalid("no model is configured for this job".into())),
        };
        let runner = deps.runner;
        if matches!(spec.pipeline, Pipeline::ExecutableQa(_)) && runner.is_none() {
            return Err(StartError::Invalid("no code sandbox is available for executable_qa".into()));
        }
        let job_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let name = spec
            .name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| format!("{}-{}", spec.pipeline.name(), chrono::Local::now().format("%Y%m%d-%H%M")));
        let owner = spec.owner.clone().filter(|o| !o.trim().is_empty());
        // The cap check and the insert share one lock, so two requests at the
        // same moment cannot both slip under it.
        let mut jobs = self.jobs.lock().unwrap();
        let running = jobs.values().filter(|h| h.status.lock().unwrap().state == JobState::Running).count();
        if running >= MAX_RUNNING {
            return Err(StartError::Busy(format!(
                "{running} datagen jobs are already running — wait for one to finish or cancel it"
            )));
        }
        let writer = DatasetWriter::create(&deps.datasets_root, &name)
            .map_err(|e| StartError::Io(format!("cannot create the dataset folder: {e}")))?;
        let dir = writer.dir().to_path_buf();
        let status = Arc::new(Mutex::new(new_status(job_id.clone(), writer.name(), spec.pipeline.name(), owner, &dir)));
        let cancel = CancellationToken::new();
        let params = serde_json::to_value(&spec).unwrap_or(Value::Null);
        write_manifest(&dir, &status.lock().unwrap(), &params);

        prune_finished(&mut jobs);
        jobs.insert(job_id.clone(), Handle { status: status.clone(), cancel: cancel.clone() });
        drop(jobs);

        let snapshot = status.lock().unwrap().clone();
        let mut ctx = JobCtx { job_id, cancel, status: status.clone(), writer };
        let trajectories_root = deps.trajectories_root;
        // The pipeline runs in its own task so a panic in it still ends the
        // job as `failed` — otherwise it would hold a running slot until the
        // daemon restarts.
        let work = tokio::spawn(async move {
            let no_model = || Err::<Outcome, String>("no model is configured for this job".into());
            match (spec.pipeline, llm) {
                (Pipeline::ExportLogs(p), _) => export_logs::run(p, &trajectories_root, &mut ctx).await,
                (_, None) => no_model(),
                (Pipeline::ExpandSeeds(p), Some(llm)) => expand_seeds::run(p, llm, &mut ctx).await,
                (Pipeline::DocQa(p), Some(llm)) => doc_qa::run(p, llm, &mut ctx).await,
                (Pipeline::ReasoningTraces(p), Some(llm)) => reasoning_traces::run(p, llm, &mut ctx).await,
                (Pipeline::ExecutableQa(p), Some(llm)) => match runner {
                    Some(runner) => executable_qa::run(p, llm, runner, &mut ctx).await,
                    None => Err("no code sandbox is available for executable_qa".into()),
                },
            }
        });
        tokio::spawn(async move {
            let result = match work.await {
                Ok(r) => r,
                Err(e) => Err(format!("the job stopped unexpectedly: {e}")),
            };
            let final_status = {
                let mut s = status.lock().unwrap();
                s.state = match &result {
                    Ok(Outcome::Finished) => JobState::Done,
                    Ok(Outcome::Cancelled) => JobState::Cancelled,
                    Err(_) => JobState::Failed,
                };
                s.error = result.err();
                s.finished_at = Some(now_ms());
                s.clone()
            };
            write_manifest(&dir, &final_status, &params);
            tracing::info!(
                "[Datagen] job {} {:?}: {} accepted, {} LLM calls",
                final_status.job_id,
                final_status.state,
                final_status.accepted,
                final_status.llm_calls
            );
        });
        Ok(snapshot)
    }

    pub fn status(&self, job_id: &str, owner: Option<&str>) -> Option<JobStatus> {
        let jobs = self.jobs.lock().unwrap();
        let s = jobs.get(job_id)?.status.lock().unwrap().clone();
        visible(owner, s.owner.as_deref()).then_some(s)
    }

    /// Jobs this process started, newest first.
    pub fn list(&self, owner: Option<&str>) -> Vec<JobStatus> {
        let jobs = self.jobs.lock().unwrap();
        let mut out: Vec<JobStatus> = jobs
            .values()
            .map(|h| h.status.lock().unwrap().clone())
            .filter(|s| visible(owner, s.owner.as_deref()))
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        out
    }

    pub fn cancel(&self, job_id: &str, owner: Option<&str>) -> CancelResult {
        let jobs = self.jobs.lock().unwrap();
        let Some(h) = jobs.get(job_id) else { return CancelResult::NotFound };
        let s = h.status.lock().unwrap();
        if !visible(owner, s.owner.as_deref()) {
            return CancelResult::NotFound;
        }
        if s.state != JobState::Running {
            return CancelResult::NotRunning;
        }
        h.cancel.cancel();
        CancelResult::Cancelled
    }

    /// Dataset manifests under `root`, with a `running` one this process is
    /// not running shown as `interrupted`.
    pub fn datasets(&self, root: &Path, owner: Option<&str>) -> Vec<Value> {
        let live: Vec<String> = self
            .list(None)
            .into_iter()
            .filter(|s| s.state == JobState::Running)
            .map(|s| s.job_id)
            .collect();
        store::list(root)
            .into_iter()
            .filter(|m| visible(owner, m.get("owner").and_then(Value::as_str)))
            .map(|mut m| {
                let running = m.get("state").and_then(Value::as_str) == Some("running");
                let id = m.get("jobId").and_then(Value::as_str).unwrap_or_default();
                if running && !live.iter().any(|j| j == id) {
                    m["state"] = json!("interrupted");
                }
                m
            })
            .collect()
    }
}

fn new_status(job_id: String, name: String, pipeline: &str, owner: Option<String>, dir: &Path) -> JobStatus {
    JobStatus {
        job_id,
        name,
        pipeline: pipeline.to_string(),
        owner,
        state: JobState::Running,
        target: 0,
        accepted: 0,
        attempts: 0,
        llm_calls: 0,
        max_llm_calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        rejected: BTreeMap::new(),
        model: None,
        note: None,
        error: None,
        dir: dir.to_string_lossy().into_owned(),
        started_at: now_ms(),
        finished_at: None,
    }
}

fn write_manifest(dir: &Path, status: &JobStatus, params: &Value) {
    let mut m = serde_json::to_value(status).unwrap_or_else(|_| json!({}));
    m["version"] = json!(1);
    m["files"] = json!([store::DATA_FILE]);
    m["params"] = params.clone();
    if let Err(e) = store::write_manifest(dir, &m) {
        tracing::warn!("[Datagen] cannot write manifest in {}: {e}", dir.display());
    }
}

fn prune_finished(jobs: &mut HashMap<String, Handle>) {
    let mut finished: Vec<(String, u64)> = jobs
        .iter()
        .filter_map(|(id, h)| {
            let s = h.status.lock().unwrap();
            (s.state != JobState::Running).then(|| (id.clone(), s.started_at))
        })
        .collect();
    if finished.len() < KEEP_FINISHED {
        return;
    }
    finished.sort_by_key(|(_, t)| *t);
    for (id, _) in finished.iter().take(finished.len() + 1 - KEEP_FINISHED) {
        jobs.remove(id);
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datagen::llm::fake::ScriptedLlm;
    use crate::datagen::llm::LlmReply;
    use async_trait::async_trait;

    fn spec(v: Value) -> JobSpec {
        serde_json::from_value(v).unwrap()
    }

    async fn wait(reg: &Registry, id: &str) -> JobStatus {
        for _ in 0..200 {
            let s = reg.status(id, None).unwrap();
            if s.state != JobState::Running {
                return s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("job {id} never finished");
    }

    fn deps(root: &Path, llm: Option<Arc<dyn Llm>>) -> Deps {
        Deps { datasets_root: root.join("datasets"), trajectories_root: root.join("trajectories"), llm, runner: None }
    }

    #[test]
    fn spec_reads_the_pipeline_tag_and_flattened_params() {
        let s = spec(json!({"pipeline": "expand_seeds", "name": "eval", "seeds": ["a b c"], "target": 5}));
        assert!(matches!(s.pipeline, Pipeline::ExpandSeeds(ref p) if p.target == 5));
        let s = spec(json!({"pipeline": "export_logs", "chats": ["web:abc"], "format": "alpaca", "owner": "web:abc"}));
        assert_eq!(s.pipeline.name(), "export_logs");
        assert_eq!(s.owner.as_deref(), Some("web:abc"));
        assert!(serde_json::from_value::<JobSpec>(json!({"pipeline": "nope"})).is_err());
        for (v, name) in [
            (json!({"pipeline": "doc_qa", "texts": ["x"]}), "doc_qa"),
            (json!({"pipeline": "reasoning_traces", "problems": ["2+2?"]}), "reasoning_traces"),
            (json!({"pipeline": "executable_qa", "seeds": ["2**10?"], "timeoutMs": 5000}), "executable_qa"),
        ] {
            assert_eq!(spec(v).pipeline.name(), name);
        }
    }

    #[test]
    fn executable_qa_needs_a_sandbox_before_anything_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let llm: Arc<dyn Llm> = Arc::new(ScriptedLlm::new(vec![]));
        let err = Registry::default()
            .start(spec(json!({"pipeline": "executable_qa", "seeds": ["2**10?"]})), deps(tmp.path(), Some(llm)))
            .unwrap_err();
        assert!(err.message().contains("sandbox"));
        assert!(!tmp.path().join("datasets").exists());
    }

    #[test]
    fn misspelled_parameters_are_refused_not_ignored() {
        let err = serde_json::from_value::<JobSpec>(json!({"pipeline": "expand_seeds", "seeds": ["a"], "max_llm_calls": 3}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_llm_calls"), "{err}");
        assert!(serde_json::from_value::<JobSpec>(json!({"pipeline": "export_logs", "chats": ["x"], "maxrecords": 1})).is_err());
    }

    #[tokio::test]
    async fn a_job_writes_its_dataset_and_a_final_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::default();
        let llm: Arc<dyn Llm> = Arc::new(ScriptedLlm::new(vec![Ok(r#"{"items":["water the plants every evening"]}"#)]));
        let st = reg
            .start(spec(json!({"pipeline": "expand_seeds", "name": "Bộ thử", "seeds": ["remind me to call mom"], "target": 1})), deps(tmp.path(), Some(llm)))
            .unwrap();
        assert_eq!(st.name, "bo-thu");
        let done = wait(&reg, &st.job_id).await;
        assert_eq!(done.state, JobState::Done);
        assert_eq!(done.accepted, 1);

        let manifests = reg.datasets(&tmp.path().join("datasets"), None);
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0]["state"], "done");
        assert_eq!(manifests[0]["params"]["pipeline"], "expand_seeds");
        assert_eq!(manifests[0]["model"], "scripted");
    }

    #[tokio::test]
    async fn bad_parameters_and_missing_models_are_refused_before_anything_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::default();
        let err = reg.start(spec(json!({"pipeline": "expand_seeds", "seeds": []})), deps(tmp.path(), None)).unwrap_err();
        assert!(matches!(err, StartError::Invalid(ref m) if m.contains("seeds is empty")));
        let err = reg
            .start(spec(json!({"pipeline": "expand_seeds", "seeds": ["x y z"]})), deps(tmp.path(), None))
            .unwrap_err();
        assert!(err.message().contains("no model"));
        assert!(!tmp.path().join("datasets").exists());
    }

    struct Never;
    #[async_trait]
    impl Llm for Never {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<LlmReply, String> {
            std::future::pending().await
        }
    }

    struct Panics;
    #[async_trait]
    impl Llm for Panics {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<LlmReply, String> {
            panic!("model adapter bug")
        }
    }

    #[tokio::test]
    async fn cancel_stops_a_job_and_caps_concurrent_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::default();
        let start = |owner: &str| {
            reg.start(
                spec(json!({"pipeline": "expand_seeds", "seeds": ["x y z"], "owner": owner})),
                deps(tmp.path(), Some(Arc::new(Never) as Arc<dyn Llm>)),
            )
        };
        let a = start("web:a").unwrap();
        let b = start("web:b").unwrap();
        assert!(matches!(start("web:a").unwrap_err(), StartError::Busy(_)));

        assert_eq!(reg.cancel(&a.job_id, Some("web:b")), CancelResult::NotFound, "another chat's job");
        assert_eq!(reg.cancel(&a.job_id, Some("web:a")), CancelResult::Cancelled);
        assert_eq!(wait(&reg, &a.job_id).await.state, JobState::Cancelled);
        assert_eq!(reg.cancel(&a.job_id, None), CancelResult::NotRunning);
        assert_eq!(reg.cancel("nope", None), CancelResult::NotFound);
        assert_eq!(reg.cancel(&b.job_id, None), CancelResult::Cancelled);
        wait(&reg, &b.job_id).await;
    }

    #[tokio::test]
    async fn a_chat_sees_only_its_own_jobs_and_datasets() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::default();
        let llm = || Some(Arc::new(ScriptedLlm::new(vec![Ok(r#"{"items":["plan a picnic for saturday"]}"#)])) as Arc<dyn Llm>);
        let a = reg.start(spec(json!({"pipeline": "expand_seeds", "seeds": ["x y z"], "target": 1, "owner": "web:a"})), deps(tmp.path(), llm())).unwrap();
        let b = reg.start(spec(json!({"pipeline": "expand_seeds", "seeds": ["x y z"], "target": 1})), deps(tmp.path(), llm())).unwrap();
        wait(&reg, &a.job_id).await;
        wait(&reg, &b.job_id).await;

        assert_eq!(reg.list(None).len(), 2);
        assert_eq!(reg.list(Some("web:a")).len(), 1);
        assert!(reg.status(&b.job_id, Some("web:a")).is_none());
        let root = tmp.path().join("datasets");
        assert_eq!(reg.datasets(&root, None).len(), 2);
        let mine = reg.datasets(&root, Some("web:a"));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["owner"], "web:a");
    }

    #[tokio::test]
    async fn a_panicking_pipeline_fails_the_job_and_frees_its_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::default();
        let st = reg
            .start(spec(json!({"pipeline": "expand_seeds", "seeds": ["x y z"]})), deps(tmp.path(), Some(Arc::new(Panics) as Arc<dyn Llm>)))
            .unwrap();
        let done = wait(&reg, &st.job_id).await;
        assert_eq!(done.state, JobState::Failed);
        assert!(done.error.unwrap().contains("stopped unexpectedly"));
        let m = reg.datasets(&tmp.path().join("datasets"), None);
        assert_eq!(m[0]["state"], "failed");
    }

    #[test]
    fn a_running_manifest_without_a_live_job_reads_as_interrupted() {
        let tmp = tempfile::tempdir().unwrap();
        let w = DatasetWriter::create(tmp.path(), "old").unwrap();
        store::write_manifest(w.dir(), &json!({"jobId": "gone", "state": "running", "startedAt": 1})).unwrap();
        let m = Registry::default().datasets(tmp.path(), None);
        assert_eq!(m[0]["state"], "interrupted");
    }
}

//! Running datagen jobs: start, watch, cancel.
//!
//! A job is a tokio task writing one dataset folder. Its live status sits in
//! a process-wide registry (a field on `UiState` would mean touching every
//! place that builds one); the folder's manifest is the durable record. A
//! manifest left at `running` with no live job is reported as `interrupted`:
//! the daemon stopped mid-run, and the records written so far are still there.
//! A job is reported `done`, `failed` or `cancelled` only after its final
//! manifest is on disk, never before.
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
use super::from_dataset::{self, DatasetRef};
use super::hf::{HfClient, PublicHf};
use super::{doc_qa, executable_qa, expand_seeds, export_logs, import, reasoning_traces};

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
    Import(import::Params),
}

impl Pipeline {
    pub fn name(&self) -> &'static str {
        match self {
            Pipeline::ExpandSeeds(_) => "expand_seeds",
            Pipeline::ExportLogs(_) => "export_logs",
            Pipeline::DocQa(_) => "doc_qa",
            Pipeline::ReasoningTraces(_) => "reasoning_traces",
            Pipeline::ExecutableQa(_) => "executable_qa",
            Pipeline::Import(_) => "import",
        }
    }

    /// The dataset name when the caller gave none.
    fn default_name(&self) -> String {
        match self {
            Pipeline::Import(p) => p.default_name(),
            _ => format!("{}-{}", self.name(), chrono::Local::now().format("%Y%m%d-%H%M")),
        }
    }

    /// Replace `seedsFrom` / `problemsFrom` with the records they name, so
    /// the pipeline and the manifest see ordinary seeds or problems.
    fn resolve_inputs(&mut self, root: &Path, owner: Option<&str>, writing: impl Fn(&str) -> bool) -> Result<(), String> {
        fn both(field: &str) -> String {
            format!("give {field} or {field}From, not both")
        }
        let pick = |r: &DatasetRef, cap: usize| from_dataset::pick(root, owner, r, cap, &writing);
        match self {
            Pipeline::ExpandSeeds(p) => {
                if let Some(r) = &p.seeds_from {
                    if !p.seeds.is_empty() {
                        return Err(both("seeds"));
                    }
                    p.seeds = from_dataset::convert(pick(r, expand_seeds::MAX_SEEDS)?, &r.dataset, "a text field")?;
                }
            }
            Pipeline::ExecutableQa(p) => {
                if let Some(r) = &p.seeds_from {
                    if !p.seeds.is_empty() {
                        return Err(both("seeds"));
                    }
                    p.seeds = from_dataset::convert(pick(r, executable_qa::MAX_SEEDS)?, &r.dataset, "a question or text field")?;
                }
            }
            Pipeline::ReasoningTraces(p) => {
                if let Some(r) = &p.problems_from {
                    if !p.problems.is_empty() {
                        return Err(both("problems"));
                    }
                    p.problems =
                        from_dataset::convert(pick(r, reasoning_traces::MAX_PROBLEMS)?, &r.dataset, "a problem or question field")?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The LLM profile the pipeline asked for, when it calls a model at all.
    pub fn profile(&self) -> Option<&str> {
        match self {
            Pipeline::ExpandSeeds(p) => p.profile.as_deref(),
            Pipeline::DocQa(p) => p.profile.as_deref(),
            Pipeline::ReasoningTraces(p) => p.profile.as_deref(),
            Pipeline::ExecutableQa(p) => p.profile.as_deref(),
            Pipeline::ExportLogs(_) | Pipeline::Import(_) => None,
        }
        .map(str::trim)
        .filter(|v| !v.is_empty())
    }

    pub fn uses_llm(&self) -> bool {
        !matches!(self, Pipeline::ExportLogs(_) | Pipeline::Import(_))
    }

    fn validate(&self, kits_dir: Option<&Path>) -> Result<(), String> {
        match self {
            Pipeline::ExpandSeeds(p) => p.plan().map(|_| ()),
            Pipeline::ExportLogs(p) => p.validate(),
            Pipeline::DocQa(p) => p.validate(),
            Pipeline::ReasoningTraces(p) => p.validate(),
            Pipeline::ExecutableQa(p) => p.validate(),
            Pipeline::Import(p) => p.validate(kits_dir),
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
    /// `import` only: where the records came from, under what license, and
    /// the sha256 of what was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Value>,
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
    /// Where `import` reads Hugging Face datasets; `None` = the public Hub.
    pub hf: Option<Arc<dyn HfClient>>,
    /// Where installed kits keep the catalog entries they registered.
    pub kits_dir: Option<PathBuf>,
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

    pub fn set_provenance(&self, provenance: Value) {
        self.status.lock().unwrap().provenance = Some(provenance);
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
    pub fn start(&self, mut spec: JobSpec, deps: Deps) -> Result<JobStatus, StartError> {
        let owner = spec.owner.clone().filter(|o| !o.trim().is_empty());
        spec.pipeline
            .resolve_inputs(&deps.datasets_root, owner.as_deref(), |job| self.is_running(job))
            .map_err(StartError::Invalid)?;
        spec.pipeline.validate(deps.kits_dir.as_deref()).map_err(StartError::Invalid)?;
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
        let name = spec.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| spec.pipeline.default_name());
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
        let hf: Arc<dyn HfClient> = deps.hf.unwrap_or_else(|| Arc::new(PublicHf::new()));
        let kits_dir = deps.kits_dir;
        // The pipeline runs in its own task so a panic in it still ends the
        // job as `failed` — otherwise it would hold a running slot until the
        // daemon restarts.
        let work = tokio::spawn(async move {
            let no_model = || Err::<Outcome, String>("no model is configured for this job".into());
            match (spec.pipeline, llm) {
                (Pipeline::ExportLogs(p), _) => export_logs::run(p, &trajectories_root, &mut ctx).await,
                (Pipeline::Import(p), _) => import::run(p, hf, kits_dir, &mut ctx).await,
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
            // The manifest is the durable record, so it is written first and
            // the live status flips after: whoever sees the job ended — the
            // REST status, a watch, a reader of the folder — finds the final
            // manifest already there. The pipeline is over, so nothing else
            // changes the status in between.
            let mut final_status = status.lock().unwrap().clone();
            final_status.state = match &result {
                Ok(Outcome::Finished) => JobState::Done,
                Ok(Outcome::Cancelled) => JobState::Cancelled,
                Err(_) => JobState::Failed,
            };
            final_status.error = result.err();
            final_status.finished_at = Some(now_ms());
            write_manifest(&dir, &final_status, &params);
            {
                let mut s = status.lock().unwrap();
                s.state = final_status.state;
                s.error = final_status.error.clone();
                s.finished_at = final_status.finished_at;
            }
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

    fn is_running(&self, job_id: &str) -> bool {
        self.status(job_id, None).is_some_and(|s| s.state == JobState::Running)
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

    /// Dataset manifests under `root` the caller may read (its own, and the
    /// catalog imports every chat may use), with a `running` one this process
    /// is not running shown as `interrupted`.
    pub fn datasets(&self, root: &Path, owner: Option<&str>) -> Vec<Value> {
        let live: Vec<String> = self
            .list(None)
            .into_iter()
            .filter(|s| s.state == JobState::Running)
            .map(|s| s.job_id)
            .collect();
        store::list(root)
            .into_iter()
            .filter(|m| from_dataset::readable_by(m, owner))
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
        provenance: None,
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
        Deps { datasets_root: root.join("datasets"), trajectories_root: root.join("trajectories"), llm, runner: None, hf: None, kits_dir: None }
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

    #[tokio::test]
    async fn an_import_is_named_after_its_source_and_its_provenance_reaches_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::default();
        let st = reg.start(spec(json!({"pipeline": "import", "source": {"catalog": "senclaw-flows-vi"}})), deps(tmp.path(), None)).unwrap();
        assert_eq!(st.name, "senclaw-flows-vi");
        let done = wait(&reg, &st.job_id).await;
        assert_eq!((done.state, done.accepted), (JobState::Done, 144));
        let root = tmp.path().join("datasets");
        let m = &reg.datasets(&root, Some("web:any-chat"))[0];
        assert_eq!(m["provenance"]["source"]["catalog"], "senclaw-flows-vi", "catalog imports are readable by every chat");
        assert_eq!(m["provenance"]["license"], "MIT");
        assert_eq!(crate::datagen::catalog::listing(&reg.datasets(&root, None), None)[0]["installed"], true);
    }

    #[tokio::test]
    async fn seeds_from_a_dataset_become_ordinary_seeds_in_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::default();
        let imp = reg.start(spec(json!({"pipeline": "import", "source": {"catalog": "senclaw-flows-vi"}})), deps(tmp.path(), None)).unwrap();
        wait(&reg, &imp.job_id).await;

        let llm: Arc<dyn Llm> = Arc::new(Never);
        let st = reg
            .start(
                spec(json!({"pipeline": "expand_seeds", "seedsFrom": {"dataset": "senclaw-flows-vi", "perLabel": 2}, "target": 5, "owner": "web:a"})),
                deps(tmp.path(), Some(llm.clone())),
            )
            .unwrap();
        let root = tmp.path().join("datasets");
        let dir = store::dataset_dir(&root, &st.name).unwrap();
        let params = &store::read_manifest(&dir).unwrap()["params"];
        assert_eq!(params["seeds"].as_array().unwrap().len(), 24, "two per label, twelve labels");
        assert_eq!(params["seedsFrom"]["dataset"], "senclaw-flows-vi");
        reg.cancel(&st.job_id, None);
        wait(&reg, &st.job_id).await;

        let both = reg
            .start(spec(json!({"pipeline": "expand_seeds", "seeds": ["x y z"], "seedsFrom": {"dataset": "senclaw-flows-vi"}})), deps(tmp.path(), Some(llm.clone())))
            .unwrap_err();
        assert!(both.message().contains("not both"));
        let wrong = reg
            .start(spec(json!({"pipeline": "reasoning_traces", "problemsFrom": {"dataset": "senclaw-flows-vi"}})), deps(tmp.path(), Some(llm)))
            .unwrap_err();
        assert!(wrong.message().contains("no records with a problem"), "{}", wrong.message());
    }

    /// How a held job ends once the test lets it.
    #[derive(Debug, Clone, Copy)]
    enum End {
        Done,
        Failed,
        Cancelled,
    }

    impl End {
        const ALL: [End; 3] = [End::Done, End::Failed, End::Cancelled];

        fn state(self) -> JobState {
            match self {
                End::Done => JobState::Done,
                End::Failed => JobState::Failed,
                End::Cancelled => JobState::Cancelled,
            }
        }
    }

    /// A model that answers (or panics) only after `gate` opens, so the test
    /// decides when the job's work is over.
    struct Held {
        gate: Arc<tokio::sync::Notify>,
        panics: bool,
    }

    #[async_trait]
    impl Llm for Held {
        async fn complete(&self, _: &str, _: &str, _: u32) -> Result<LlmReply, String> {
            self.gate.notified().await;
            if self.panics {
                panic!("model adapter bug");
            }
            Ok(LlmReply { text: r#"{"items":["water the plants every evening"]}"#.into(), model: "scripted".into(), ..Default::default() })
        }
    }

    /// Start an `expand_seeds` job that waits on its model; `release` ends it as `end` says.
    fn start_held(reg: &Registry, root: &Path, end: End) -> (JobStatus, Arc<tokio::sync::Notify>) {
        let gate = Arc::new(tokio::sync::Notify::new());
        let llm: Arc<dyn Llm> = Arc::new(Held { gate: gate.clone(), panics: matches!(end, End::Failed) });
        let st = reg
            .start(spec(json!({"pipeline": "expand_seeds", "seeds": ["remind me to call mom"], "target": 1})), deps(root, Some(llm)))
            .unwrap();
        (st, gate)
    }

    fn release(reg: &Registry, st: &JobStatus, gate: &tokio::sync::Notify, end: End) {
        match end {
            End::Cancelled => assert_eq!(reg.cancel(&st.job_id, None), CancelResult::Cancelled),
            End::Done | End::Failed => gate.notify_one(),
        }
    }

    fn manifest_on_disk(dir: &str) -> Value {
        store::read_manifest(Path::new(dir)).expect("manifest.json is readable")
    }

    /// The manifest is the durable record, so a job the registry reports as
    /// ended must already have it written. The final write is stalled here —
    /// opening a FIFO for writing waits for a reader, and the writer meets
    /// the FIFO where it expects its temp file — so the registry cannot
    /// have got ahead of it by luck of timing.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_job_is_not_reported_ended_while_its_manifest_is_still_being_written() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;

        for end in End::ALL {
            let tmp = tempfile::tempdir().unwrap();
            let reg = Registry::default();
            let (st, gate) = start_held(&reg, tmp.path(), end);
            let stall = std::ffi::CString::new(Path::new(&st.dir).join(store::MANIFEST_TMP).as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(stall.as_ptr(), 0o600) }, 0, "{end:?}: mkfifo");
            release(&reg, &st, &gate, end);

            let mut seen_ended = None;
            for _ in 0..40 {
                let s = reg.status(&st.job_id, None).unwrap();
                if s.state != JobState::Running {
                    seen_ended = Some((s.state, manifest_on_disk(&st.dir)["state"].clone()));
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }

            // Let the stalled write through (a FIFO cannot be synced, so it
            // fails and the job finishes without a manifest) before asserting,
            // so a failure here cannot leave a worker thread blocked forever.
            let reader = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(Path::new(&st.dir).join(store::MANIFEST_TMP)).unwrap();
            let ended = wait(&reg, &st.job_id).await;
            drop(reader);

            assert_eq!(ended.state, end.state());
            if let Some((state, on_disk)) = seen_ended {
                assert_eq!(on_disk, json!(state), "{end:?}: the registry said {state:?} while manifest.json said {on_disk}");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_job_seen_ended_already_has_its_final_manifest_on_disk() {
        for round in 0..5 {
            for end in End::ALL {
                let tmp = tempfile::tempdir().unwrap();
                let reg = Registry::default();
                let (st, gate) = start_held(&reg, tmp.path(), end);
                release(&reg, &st, &gate, end);
                let ended = loop {
                    let s = reg.status(&st.job_id, None).unwrap();
                    if s.state != JobState::Running {
                        break s;
                    }
                    tokio::task::yield_now().await;
                };
                // Read the file the instant the state is seen, with no wait.
                let m = manifest_on_disk(&st.dir);
                let at = format!("round {round}, {end:?}");
                assert_eq!(m["state"], json!(ended.state), "{at}");
                assert_eq!(m["accepted"], json!(ended.accepted), "{at}");
                assert_eq!(m["finishedAt"], json!(ended.finished_at), "{at}");
                assert!(m["finishedAt"].is_u64(), "{at}");
                assert_eq!(m["error"], json!(ended.error), "{at}");
                match end {
                    End::Done => assert_eq!(m["accepted"], 1, "{at}"),
                    End::Failed => assert!(m["error"].as_str().is_some_and(|e| e.contains("stopped unexpectedly")), "{at}"),
                    End::Cancelled => assert!(m["error"].is_null(), "{at}"),
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn an_import_seen_done_already_has_its_provenance_on_disk() {
        for round in 0..5 {
            let tmp = tempfile::tempdir().unwrap();
            let reg = Registry::default();
            let st = reg.start(spec(json!({"pipeline": "import", "source": {"catalog": "senclaw-flows-vi"}})), deps(tmp.path(), None)).unwrap();
            let ended = loop {
                let s = reg.status(&st.job_id, None).unwrap();
                if s.state != JobState::Running {
                    break s;
                }
                tokio::task::yield_now().await;
            };
            let m = manifest_on_disk(&st.dir);
            assert_eq!(ended.state, JobState::Done, "round {round}");
            assert_eq!(m["state"], "done", "round {round}: the registry said done while manifest.json said {}", m["state"]);
            assert_eq!(m["accepted"], 144, "round {round}");
            assert_eq!(m["provenance"]["license"], "MIT", "round {round}");
        }
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

//! Background install jobs (`docs/runtime-protocol.md` §5.1).
//!
//! `POST /api/runtimes/install` returns a `jobId` at once; the actual
//! download + extraction runs on a spawned task and is polled at
//! `GET /api/runtimes/jobs/:jobId`. One job per `(id, version)` at a time —
//! a second install request for the same target returns the job already
//! running rather than starting a competing download into the same
//! destination.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::index::RuntimeIndex;
use super::llamacpp;
use super::store::{self, InstalledPackage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Downloading,
    Verifying,
    Extracting,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    pub job_id: String,
    pub id: String,
    pub version: String,
    pub state: JobState,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub percent: Option<f64>,
    pub error: Option<String>,
    pub started_at: u64,
    pub finished_at: Option<u64>,
}

impl JobStatus {
    fn new(job_id: String, id: String, version: String) -> JobStatus {
        JobStatus {
            job_id,
            id,
            version,
            state: JobState::Queued,
            received_bytes: 0,
            total_bytes: None,
            percent: None,
            error: None,
            started_at: now_millis(),
            finished_at: None,
        }
    }

    fn is_finished(&self) -> bool {
        matches!(self.state, JobState::Done | JobState::Failed | JobState::Cancelled)
    }
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

struct Handle {
    status: Arc<Mutex<JobStatus>>,
    cancel: CancellationToken,
}

/// All install jobs this process has started, keyed by job id.
#[derive(Default)]
pub struct JobRegistry {
    jobs: Mutex<HashMap<String, Handle>>,
}

impl JobRegistry {
    pub fn new() -> JobRegistry {
        JobRegistry::default()
    }

    /// Snapshot of one job.
    pub fn status(&self, job_id: &str) -> Option<JobStatus> {
        let jobs = self.jobs.lock().unwrap();
        jobs.get(job_id).map(|h| h.status.lock().unwrap().clone())
    }

    /// Every job this process knows about, newest first.
    pub fn list(&self) -> Vec<JobStatus> {
        let jobs = self.jobs.lock().unwrap();
        let mut out: Vec<JobStatus> = jobs.values().map(|h| h.status.lock().unwrap().clone()).collect();
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        out
    }

    /// An in-flight job for this exact `(id, version)`, if one is running.
    fn running_for(&self, id: &str, version: &str) -> Option<JobStatus> {
        let jobs = self.jobs.lock().unwrap();
        jobs.values().find_map(|h| {
            let s = h.status.lock().unwrap();
            (s.id == id && s.version == version && !s.is_finished()).then(|| s.clone())
        })
    }

    pub fn cancel(&self, job_id: &str) -> bool {
        let jobs = self.jobs.lock().unwrap();
        match jobs.get(job_id) {
            Some(h) => {
                h.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Start installing `id` at `version` (a concrete version, or the
    /// resolved index/channel version by the time this is called) into
    /// `runtimes_dir`. Idempotent: a second call for the same `(id, version)`
    /// while one is still running returns that job instead of starting a
    /// second writer.
    pub fn start(
        self: &Arc<Self>,
        runtimes_dir: std::path::PathBuf,
        index: RuntimeIndex,
        id: String,
        version: String,
    ) -> JobStatus {
        if let Some(existing) = self.running_for(&id, &version) {
            return existing;
        }
        let job_id = uuid::Uuid::new_v4().to_string();
        let status = Arc::new(Mutex::new(JobStatus::new(job_id.clone(), id.clone(), version.clone())));
        let cancel = CancellationToken::new();
        self.jobs.lock().unwrap().insert(
            job_id.clone(),
            Handle { status: Arc::clone(&status), cancel: cancel.clone() },
        );

        tokio::spawn(run_job(runtimes_dir, index, id, version, Arc::clone(&status), cancel));

        let snapshot = status.lock().unwrap().clone();
        snapshot
    }
}

async fn run_job(
    runtimes_dir: std::path::PathBuf,
    index: RuntimeIndex,
    id: String,
    version: String,
    status: Arc<Mutex<JobStatus>>,
    cancel: CancellationToken,
) {
    let set = |f: &dyn Fn(&mut JobStatus)| f(&mut status.lock().unwrap());
    if cancel.is_cancelled() {
        set(&|s| {
            s.state = JobState::Cancelled;
            s.finished_at.get_or_insert_with(now_millis);
        });
        return;
    }
    let result = install_one(&runtimes_dir, &index, &id, &version, &status, &cancel).await;
    let mut s = status.lock().unwrap();
    if s.state == JobState::Cancelled || cancel.is_cancelled() {
        // Either the GGUF path already set `Cancelled` directly, or the
        // llama.cpp path (which only knows how to bail with an error) noticed
        // the token — either way this is a cancellation, not a failure, and
        // it still needs `finishedAt` set exactly once.
        s.state = JobState::Cancelled;
        s.finished_at.get_or_insert_with(now_millis);
        return;
    }
    s.finished_at = Some(now_millis());
    match result {
        Ok(_) => {
            s.state = JobState::Done;
            s.percent = Some(100.0);
        }
        Err(e) => {
            s.state = JobState::Failed;
            s.error = Some(format!("{e:#}"));
        }
    }
}

async fn install_one(
    runtimes_dir: &std::path::Path,
    index: &RuntimeIndex,
    id: &str,
    version: &str,
    status: &Arc<Mutex<JobStatus>>,
    cancel: &CancellationToken,
) -> anyhow::Result<InstalledPackage> {
    let entry = index.entry(id).ok_or_else(|| anyhow::anyhow!("`{id}` is not in the runtime index"))?;
    {
        let mut s = status.lock().unwrap();
        s.state = JobState::Downloading;
    }

    let on_progress = {
        let status = Arc::clone(status);
        move |received: u64, total: Option<u64>| {
            let mut s = status.lock().unwrap();
            s.received_bytes = received;
            s.total_bytes = total.or(s.total_bytes);
            s.percent = s.total_bytes.filter(|t| *t > 0).map(|t| (received as f64 / t as f64 * 100.0).min(99.0));
        }
    };

    if entry.upstream.is_some() {
        let pkg = llamacpp::install(runtimes_dir, entry, version, &on_progress, cancel).await?;
        return Ok(pkg);
    }

    let platform = sen_runtime_sdk::platform::current();
    let release = entry
        .release(version)
        .ok_or_else(|| anyhow::anyhow!("`{id}` has no published release `{version}`"))?;
    let package = entry
        .package_for(release, platform)
        .ok_or_else(|| anyhow::anyhow!("`{id}` `{version}` has no package for `{platform}`"))?;

    let staged_parent = store::scratch_dir(runtimes_dir)?;
    let archive_name = package.url.rsplit('/').next().unwrap_or("package.tar.gz");
    let archive_path = staged_parent.join(archive_name);
    let download = download_with_progress(&package.url, &archive_path, cancel, &on_progress).await;
    if let Err(e) = download {
        let _ = std::fs::remove_dir_all(&staged_parent);
        return Err(e);
    }
    if cancel.is_cancelled() {
        let _ = std::fs::remove_dir_all(&staged_parent);
        status.lock().unwrap().state = JobState::Cancelled;
        anyhow::bail!("cancelled");
    }

    {
        let mut s = status.lock().unwrap();
        s.state = JobState::Verifying;
    }
    if let Some(expected) = &package.sha256 {
        verify_sha256(&archive_path, expected)?;
    }

    {
        let mut s = status.lock().unwrap();
        s.state = JobState::Extracting;
    }
    let outcome = store::install_from_archive(runtimes_dir, &archive_path, store::PackageSource::Index);
    let _ = std::fs::remove_dir_all(&staged_parent);
    outcome
}

async fn download_with_progress(
    url: &str,
    dest: &std::path::Path,
    cancel: &CancellationToken,
    on_progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let client = reqwest::Client::builder().user_agent(format!("senclaw/{}", env!("CARGO_PKG_VERSION"))).build()?;
    let mut resp = client.get(url).send().await?.error_for_status()?;
    let total = resp.content_length();
    let mut file = tokio::fs::File::create(dest).await?;
    let mut received: u64 = 0;
    while let Some(chunk) = resp.chunk().await? {
        if cancel.is_cancelled() {
            return Ok(());
        }
        file.write_all(&chunk).await?;
        received += chunk.len() as u64;
        on_progress(received, total);
    }
    file.flush().await?;
    Ok(())
}

fn verify_sha256(path: &std::path::Path, expected: &str) -> anyhow::Result<()> {
    use sha2::Digest;
    let bytes = std::fs::read(path)?;
    let got = hex::encode(sha2::Sha256::digest(&bytes));
    if !got.eq_ignore_ascii_case(expected) {
        anyhow::bail!("sha256 mismatch: expected {expected}, got {got}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cancelled job used to leave `finishedAt: null` forever — it looked,
    /// from the outside, like a job that never stopped running.
    #[tokio::test]
    async fn a_cancelled_job_is_terminal_with_finished_at_set() {
        let status = Arc::new(Mutex::new(JobStatus::new("j1".into(), "sen-ocr".into(), "0.1.0".into())));
        let cancel = CancellationToken::new();
        cancel.cancel(); // already cancelled before run_job ever looks at it
        run_job(std::path::PathBuf::from("/tmp"), RuntimeIndex::bundled(), "sen-ocr".into(), "0.1.0".into(), Arc::clone(&status), cancel)
            .await;
        let s = status.lock().unwrap();
        assert_eq!(s.state, JobState::Cancelled);
        assert!(s.finished_at.is_some(), "a cancelled job must still report when it finished");
    }

    #[test]
    fn a_job_starts_queued_and_reports_terminal_states() {
        let mut s = JobStatus::new("j1".into(), "sen-ocr".into(), "0.1.0".into());
        assert_eq!(s.state, JobState::Queued);
        assert!(!s.is_finished());
        for st in [JobState::Done, JobState::Failed, JobState::Cancelled] {
            s.state = st;
            assert!(s.is_finished());
        }
    }

    #[tokio::test]
    async fn starting_the_same_target_twice_returns_the_same_job() {
        let registry = Arc::new(JobRegistry::new());
        let tmp = tempfile::tempdir().unwrap();
        let index = RuntimeIndex::parse(
            r#"{"schemaVersion":1,"runtimes":[{"id":"sen-ocr","name":"OCR","type":"ocr",
                "slots":["ocr"],"capabilities":["ocr"],"platforms":["darwin-arm64"],
                "channels":{"stable":"0.1.0"},"releases":[]}]}"#,
        )
        .unwrap();
        let first = registry.start(tmp.path().to_path_buf(), index.clone(), "sen-ocr".into(), "0.1.0".into());
        let second = registry.start(tmp.path().to_path_buf(), index, "sen-ocr".into(), "0.1.0".into());
        assert_eq!(first.job_id, second.job_id);
    }

    #[tokio::test]
    async fn a_release_with_no_package_for_this_platform_fails_the_job() {
        let registry = Arc::new(JobRegistry::new());
        let tmp = tempfile::tempdir().unwrap();
        let index = RuntimeIndex::parse(
            r#"{"schemaVersion":1,"runtimes":[{"id":"sen-ocr","name":"OCR","type":"ocr",
                "slots":["ocr"],"capabilities":["ocr"],"platforms":["darwin-arm64","linux-x64"],
                "channels":{"stable":"0.1.0"},
                "releases":[{"version":"0.1.0","packages":[{"platform":"linux-x64","url":"https://example.invalid/x.tar.gz"}]}]}]}"#,
        )
        .unwrap();
        let job = registry.start(tmp.path().to_path_buf(), index, "sen-ocr".into(), "0.1.0".into());
        // Poll until the spawned task finishes (bounded — this never talks to
        // the network, so resolution is immediate once scheduled).
        let mut status = job.clone();
        for _ in 0..200 {
            if let Some(s) = registry.status(&job.job_id) {
                status = s;
                if status.is_finished() {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(status.state, JobState::Failed);
        assert!(status.error.unwrap().contains("no package"));
    }
}

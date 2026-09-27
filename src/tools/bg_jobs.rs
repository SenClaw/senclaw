//! Background job manager for [`BashTool`] (and future async tools).
//!
//! Mirrors sema-core's `TaskManager` — a process-wide registry of jobs that
//! were launched in the background. Used by `PeekBgJob` to fetch output and
//! `StopBgJob` to terminate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::process::Child;
use tokio::sync::{broadcast, Notify};

/// Lifecycle of a background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Done,
    Failed,
    Stopped,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
            JobStatus::Stopped => "stopped",
        }
    }
}

/// Job kind — used by the peek tool to decide whether to stream chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Bash,
    SubAgent,
}

impl JobKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobKind::Bash => "Bash",
            JobKind::SubAgent => "SubAgent",
        }
    }
}

pub struct BgJob {
    pub id: String,
    pub kind: JobKind,
    pub command: String,
    pub started_at: Instant,
    /// Accumulated stdout/stderr (capped at 1 MiB).
    output: Mutex<String>,
    status: Mutex<JobStatus>,
    /// Broadcast channel for output deltas (chunked text).
    chunk_tx: broadcast::Sender<String>,
    /// Signaled when the job transitions out of `Running`.
    finished: Arc<Notify>,
    /// Optional handle to terminate the underlying child process.
    child: Mutex<Option<Child>>,
}

impl BgJob {
    pub fn new(id: String, kind: JobKind, command: String) -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(256);
        Arc::new(Self {
            id,
            kind,
            command,
            started_at: Instant::now(),
            output: Mutex::new(String::new()),
            status: Mutex::new(JobStatus::Running),
            chunk_tx: tx,
            finished: Arc::new(Notify::new()),
            child: Mutex::new(None),
        })
    }

    pub fn status(&self) -> JobStatus {
        *self.status.lock().unwrap()
    }

    pub fn output_snapshot(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    pub fn append_output(&self, chunk: &str) {
        const MAX_OUTPUT: usize = 1024 * 1024;
        let mut buf = self.output.lock().unwrap();
        let remaining = MAX_OUTPUT.saturating_sub(buf.len());
        if remaining > 0 {
            let slice = if chunk.len() <= remaining {
                chunk
            } else {
                // Find safe utf-8 boundary
                let mut end = remaining;
                while end > 0 && !chunk.is_char_boundary(end) {
                    end -= 1;
                }
                &chunk[..end]
            };
            buf.push_str(slice);
        }
        let _ = self.chunk_tx.send(chunk.to_string());
    }

    pub fn subscribe_chunks(&self) -> broadcast::Receiver<String> {
        self.chunk_tx.subscribe()
    }

    pub fn set_child(&self, child: Child) {
        *self.child.lock().unwrap() = Some(child);
    }

    /// Mark a terminal status and wake all waiters. Idempotent.
    pub fn mark_done(&self, status: JobStatus) {
        let mut s = self.status.lock().unwrap();
        if *s == JobStatus::Running {
            *s = status;
            self.finished.notify_waiters();
        }
    }

    /// Force-kill the child process (if any) and mark stopped.
    pub async fn kill(&self) -> bool {
        let child = self.child.lock().unwrap().take();
        let killed = if let Some(mut c) = child {
            c.kill().await.is_ok()
        } else {
            false
        };
        self.mark_done(JobStatus::Stopped);
        killed
    }
}

// ============================================================================
// Process-wide manager
// ============================================================================

pub struct BgJobManager {
    jobs: Mutex<HashMap<String, Arc<BgJob>>>,
    counter: Mutex<u64>,
}

static MANAGER: OnceLock<Arc<BgJobManager>> = OnceLock::new();

impl BgJobManager {
    /// Process-wide singleton. Lazily initialised on first call.
    pub fn global() -> Arc<Self> {
        MANAGER
            .get_or_init(|| {
                Arc::new(Self {
                    jobs: Mutex::new(HashMap::new()),
                    counter: Mutex::new(0),
                })
            })
            .clone()
    }

    /// Allocate a fresh ID like `bg-NNNNN`.
    pub fn next_id(&self, kind: JobKind) -> String {
        let mut n = self.counter.lock().unwrap();
        *n += 1;
        format!("{}-{:05}", kind.as_str().to_lowercase(), *n)
    }

    pub fn register(&self, job: Arc<BgJob>) {
        self.jobs.lock().unwrap().insert(job.id.clone(), job);
    }

    pub fn get(&self, id: &str) -> Option<Arc<BgJob>> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<BgJob>> {
        self.jobs.lock().unwrap().values().cloned().collect()
    }

    /// Wait for a job to leave `Running`, up to `timeout`. Returns the final
    /// status, or `JobStatus::Running` if it timed out.
    pub async fn wait(&self, id: &str, timeout: Duration) -> Option<JobStatus> {
        let job = self.get(id)?;
        if job.status() != JobStatus::Running {
            return Some(job.status());
        }
        let finished = job.finished.clone();
        let _ = tokio::time::timeout(timeout, finished.notified()).await;
        Some(job.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_get() {
        let mgr = BgJobManager::global();
        let id = mgr.next_id(JobKind::Bash);
        let job = BgJob::new(id.clone(), JobKind::Bash, "echo hi".to_string());
        mgr.register(job.clone());
        let fetched = mgr.get(&id).unwrap();
        assert_eq!(fetched.id, id);
        assert_eq!(fetched.status(), JobStatus::Running);
    }

    #[tokio::test]
    async fn mark_done_wakes_wait() {
        let mgr = BgJobManager::global();
        let id = mgr.next_id(JobKind::Bash);
        let job = BgJob::new(id.clone(), JobKind::Bash, "sleep 1".to_string());
        mgr.register(job.clone());
        let job_for_thread = job.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            job_for_thread.mark_done(JobStatus::Done);
        });
        let status = mgr.wait(&id, Duration::from_millis(500)).await.unwrap();
        assert_eq!(status, JobStatus::Done);
    }

    #[tokio::test]
    async fn wait_returns_running_on_timeout() {
        let mgr = BgJobManager::global();
        let id = mgr.next_id(JobKind::Bash);
        let job = BgJob::new(id.clone(), JobKind::Bash, "stuck".to_string());
        mgr.register(job.clone());
        let status = mgr.wait(&id, Duration::from_millis(20)).await.unwrap();
        assert_eq!(status, JobStatus::Running);
    }

    #[test]
    fn append_output_caps_size() {
        let job = BgJob::new("t".into(), JobKind::Bash, "x".into());
        let big = "a".repeat(2_000_000);
        job.append_output(&big);
        let snap = job.output_snapshot();
        assert!(snap.len() <= 1024 * 1024);
    }

    #[test]
    fn unique_ids_per_kind() {
        let mgr = BgJobManager::global();
        let a = mgr.next_id(JobKind::Bash);
        let b = mgr.next_id(JobKind::Bash);
        assert_ne!(a, b);
        assert!(a.starts_with("bash-"));
    }
}

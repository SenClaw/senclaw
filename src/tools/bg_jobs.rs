//! Background job manager for [`BashTool`] (and future async tools).
//!
//! Mirrors sema-core's `TaskManager` — a process-wide registry of jobs that
//! were launched in the background. Used by `PeekBgJob` to fetch output and
//! `StopBgJob` to terminate, and by `/api/bg-tasks` to show a person what an
//! agent left running (see [`crate::bg_tasks`], which creates, adopts, watches
//! and stops the entries).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// How a job came to be in the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The daemon started it (`Bash` with `run_in_background`).
    Spawned,
    /// A foreground command left it running; found in the shell's process group.
    Adopted,
    /// Read back from `bg-tasks.json` after a daemon restart.
    Restored,
}

impl Origin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Origin::Spawned => "spawned",
            Origin::Adopted => "adopted",
            Origin::Restored => "restored",
        }
    }
}

/// What a person needs to recognise a job and to know whose it is. Immutable.
#[derive(Debug, Clone)]
pub struct JobMeta {
    /// The chat whose agent started it; empty when started outside a chat.
    pub chat_jid: String,
    /// The agent's own one-line description of the command.
    pub description: String,
    pub cwd: String,
    pub origin: Origin,
    /// `seatbelt` / `bubblewrap` / … when the command ran in the OS sandbox.
    pub isolation: String,
    /// Wall clock, ms since the epoch (`started_at` is monotonic and cannot be shown).
    pub started_ms: i64,
}

impl Default for JobMeta {
    fn default() -> Self {
        Self {
            chat_jid: String::new(),
            description: String::new(),
            cwd: String::new(),
            origin: Origin::Spawned,
            isolation: String::new(),
            started_ms: now_ms(),
        }
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// What changes while a job runs.
#[derive(Debug, Default, Clone)]
pub struct JobState {
    /// The process group the job lives in (Unix). Its leader's pid, because the
    /// shell is a session leader (`setsid`).
    pub pgid: Option<u32>,
    /// The pid the job was started as; on Windows the only handle there is.
    pub pid: Option<u32>,
    /// Live members of the group, refreshed by the watcher.
    pub pids: Vec<u32>,
    /// TCP ports the group listens on, refreshed by the watcher.
    pub ports: Vec<u16>,
    pub exit_code: Option<i32>,
    pub ended_ms: Option<i64>,
    /// The started shell has exited. The job runs on while its group has a member.
    pub child_exited: bool,
}

pub struct BgJob {
    pub id: String,
    pub kind: JobKind,
    pub command: String,
    pub meta: JobMeta,
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
    state: Mutex<JobState>,
    /// A stop was asked for: the exit that follows is "stopped", not "failed".
    stopping: AtomicBool,
}

impl BgJob {
    pub fn new(id: String, kind: JobKind, command: String) -> Arc<Self> {
        Self::with_meta(id, kind, command, JobMeta::default())
    }

    pub fn with_meta(id: String, kind: JobKind, command: String, meta: JobMeta) -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(256);
        Arc::new(Self {
            id,
            kind,
            command,
            meta,
            started_at: Instant::now(),
            output: Mutex::new(String::new()),
            status: Mutex::new(JobStatus::Running),
            chunk_tx: tx,
            finished: Arc::new(Notify::new()),
            child: Mutex::new(None),
            state: Mutex::new(JobState::default()),
            stopping: AtomicBool::new(false),
        })
    }

    pub fn state(&self) -> JobState {
        self.state.lock().unwrap().clone()
    }

    pub fn update_state(&self, f: impl FnOnce(&mut JobState)) {
        f(&mut self.state.lock().unwrap());
    }

    pub fn pgid(&self) -> Option<u32> {
        self.state.lock().unwrap().pgid
    }

    pub fn stop_requested(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    pub fn request_stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    /// The job is over. A requested stop wins over the exit code: SIGTERM ends a
    /// shell with 143, which must read "stopped", not "failed".
    pub fn finish(&self, exit_code: Option<i32>) {
        if let Some(c) = exit_code {
            self.update_state(|s| s.exit_code = Some(c));
        }
        let code = self.state.lock().unwrap().exit_code;
        let status = if self.stop_requested() {
            JobStatus::Stopped
        } else {
            match code {
                Some(0) | None => JobStatus::Done,
                Some(_) => JobStatus::Failed,
            }
        };
        self.mark_done(status);
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
            self.state.lock().unwrap().ended_ms = Some(now_ms());
            self.finished.notify_waiters();
        }
    }

    /// Stop the job: the whole process group when it has one (a dev server is a
    /// shell, npm and node, and killing the head leaves the rest), else the child.
    /// True when something was signalled and is gone.
    pub async fn kill(&self) -> bool {
        self.request_stop();
        if crate::bg_tasks::has_group(self) {
            return crate::bg_tasks::stop(self).await;
        }
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

    /// After a restart, ids read back from disk must not be handed out again.
    pub fn bump_counter_past(&self, id: &str) {
        let n: u64 = id.rsplit('-').next().and_then(|d| d.parse().ok()).unwrap_or(0);
        let mut c = self.counter.lock().unwrap();
        *c = (*c).max(n);
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

    /// Forget a job. Only a finished one: a running job is stopped first.
    pub fn remove(&self, id: &str) -> bool {
        let mut jobs = self.jobs.lock().unwrap();
        match jobs.get(id) {
            Some(j) if j.status() != JobStatus::Running => jobs.remove(id).is_some(),
            _ => false,
        }
    }

    /// Keep the table bounded: finished jobs older than `max_age` go, and so do
    /// the oldest ones beyond `keep`. Running jobs are never dropped.
    pub fn prune_finished(&self, max_age: Duration, keep: usize) {
        let now = now_ms();
        let mut jobs = self.jobs.lock().unwrap();
        let mut finished: Vec<(String, i64)> = jobs
            .values()
            .filter(|j| j.status() != JobStatus::Running)
            .map(|j| (j.id.clone(), j.state().ended_ms.unwrap_or(j.meta.started_ms)))
            .collect();
        finished.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
        for (i, (id, ended)) in finished.iter().enumerate() {
            if i >= keep || now - ended > max_age.as_millis() as i64 {
                jobs.remove(id);
            }
        }
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

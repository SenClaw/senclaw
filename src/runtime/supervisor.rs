//! Spawns, health-gates and supervises runtime processes
//! (`docs/runtime-protocol.md` §3.2).
//!
//! One process per running key: `service:<runtime-id>` for a service
//! runtime, `model:<model-key>` for a loaded model. A start is single-flight
//! per key (every concurrent caller for the same key awaits the same spawn);
//! idle processes are swept on a timer; `running.json` lets a fresh daemon
//! clean up whatever the previous one left behind.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::RngCore;
use sen_runtime_sdk::manifest::{Capability, RuntimeManifest};
use serde::{Deserialize, Serialize};

/// What the caller wants started.
pub struct LaunchSpec {
    pub manifest: RuntimeManifest,
    pub package_dir: PathBuf,
    pub data_dir: PathBuf,
    pub models_dir: PathBuf,
    pub config_path: PathBuf,
    pub home: PathBuf,
    pub log_path: PathBuf,
    /// Model mode only.
    pub model: Option<ModelLaunch>,
    /// `None` = the mode's default (service 300s, model 900s); `Some(0)` = never.
    pub idle_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ModelLaunch {
    pub id: String,
    pub path: PathBuf,
    pub mmproj_path: Option<PathBuf>,
    pub context_length: u32,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessState {
    Starting,
    Ready,
    Stopping,
    Failed,
}

/// A tracked child process.
pub struct RunningProcess {
    pub key: String,
    pub runtime_id: String,
    pub version: String,
    pub port: u16,
    pub token: String,
    pub package_dir: PathBuf,
    pub started_at: u64,
    pub last_used_at: AtomicU64,
    pub launches: AtomicU32,
    pub in_flight: AtomicU32,
    /// The manifest's own override (`docs/runtime-protocol.md` §3.1):
    /// `None` → the daemon default for this process's mode, `Some(0)` →
    /// never idle out. Captured at spawn time from `LaunchSpec`.
    pub idle_timeout_secs: Option<u64>,
    state: Mutex<ProcessState>,
    error: Mutex<Option<String>>,
    child: Mutex<Option<tokio::process::Child>>,
    pid: u32,
    // The manifest's own shutdown endpoint may not exist (upstream
    // llama.cpp): a 404/timeout there falls through to the OS signal either
    // way, so nothing here needs to know which binary this is.
}

/// The JSON view for `GET /api/runtimes` and the health-gate error path.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSnapshot {
    pub key: String,
    pub runtime_id: String,
    pub version: String,
    pub pid: u32,
    pub port: u16,
    pub state: ProcessState,
    pub started_at: u64,
    pub last_used_at: u64,
    pub launches: u32,
    pub error: Option<String>,
}

impl RunningProcess {
    pub fn snapshot(&self) -> ProcessSnapshot {
        ProcessSnapshot {
            key: self.key.clone(),
            runtime_id: self.runtime_id.clone(),
            version: self.version.clone(),
            pid: self.pid,
            port: self.port,
            state: *self.state.lock().unwrap(),
            started_at: self.started_at,
            last_used_at: self.last_used_at.load(Ordering::Relaxed),
            launches: self.launches.load(Ordering::Relaxed),
            error: self.error.lock().unwrap().clone(),
        }
    }

    /// The one process shape used everywhere (`docs/runtime-protocol.md`
    /// §5.1): `GET /api/runtimes` `processes[]`, `POST /api/local-models/:key/load`,
    /// `POST /api/runtimes/slots/:slot/start` and `LocalModel.process` all
    /// carry exactly this. `slot`/`model_key` are supplied by the caller — a
    /// process does not itself know which slot it fills, and only a
    /// `model:`-keyed one has a model key at all.
    pub fn view(&self, slot: Option<&str>, model_key: Option<&str>) -> serde_json::Value {
        let snap = self.snapshot();
        serde_json::json!({
            "key": snap.key, "runtimeId": snap.runtime_id, "version": snap.version, "slot": slot,
            "modelKey": model_key, "pid": snap.pid, "port": snap.port, "state": snap.state,
            "startedAt": snap.started_at, "lastUsedAt": snap.last_used_at, "launches": snap.launches,
            "error": snap.error,
        })
    }

    pub fn touch(&self) {
        self.last_used_at.store(now_millis(), Ordering::Relaxed);
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn is_ready(&self) -> bool {
        *self.state.lock().unwrap() == ProcessState::Ready
    }

    /// Non-blocking check for whether the OS process has actually exited —
    /// used to detect a crash that happened *after* the health gate passed
    /// (`docs/runtime-protocol.md` §3.2 step 9), since nothing here polls for
    /// that on its own; it is only ever checked reactively, from a request
    /// path.
    fn try_wait_exited(&self) -> Option<std::process::ExitStatus> {
        let mut guard = self.child.lock().unwrap();
        guard.as_mut().and_then(|c| c.try_wait().ok()).flatten()
    }
}

#[cfg(test)]
impl Supervisor {
    /// Test-only: track a process with no real child under `key`, for
    /// exercising request bookkeeping (`begin_request`/`end_request`,
    /// `in_flight`) from another module's tests without spawning a real OS
    /// process. Returns the tracked `Arc` so the caller can assert on it.
    pub(crate) fn track_fake_for_test(&self, key: &str) -> Arc<RunningProcess> {
        self.track_fake_for_test_as(key, "test", "0.0.0")
    }

    /// [`Self::track_fake_for_test`] for a given runtime id and version.
    pub(crate) fn track_fake_for_test_as(&self, key: &str, runtime_id: &str, version: &str) -> Arc<RunningProcess> {
        let proc = Arc::new(RunningProcess {
            key: key.to_string(),
            runtime_id: runtime_id.into(),
            version: version.into(),
            port: 0,
            token: String::new(),
            package_dir: PathBuf::new(),
            started_at: now_millis(),
            last_used_at: AtomicU64::new(now_millis()),
            launches: AtomicU32::new(1),
            in_flight: AtomicU32::new(0),
            idle_timeout_secs: None,
            state: Mutex::new(ProcessState::Ready),
            error: Mutex::new(None),
            child: Mutex::new(None),
            pid: 0,
        });
        self.processes.lock().unwrap().insert(key.to_string(), Arc::clone(&proc));
        proc
    }
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// One `running.json` entry — enough to find and stop an orphan after a
/// restart, without re-parsing the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunningEntry {
    key: String,
    pid: u32,
    port: u16,
    id: String,
    version: String,
    package_dir: PathBuf,
    started_at: u64,
}

pub enum StartError {
    /// `RuntimeManifest::launch_args`/`launch_env` rejected a placeholder —
    /// should never happen for a validated manifest, but the caller still
    /// needs a typed answer rather than a panic.
    BadManifest(String),
    Io(String),
    /// The process exited, or never answered healthy, before the timeout.
    /// Carries the tail of its log for the error message.
    Unhealthy { log_tail: String },
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::BadManifest(e) => write!(f, "invalid manifest: {e}"),
            StartError::Io(e) => write!(f, "could not start the process: {e}"),
            StartError::Unhealthy { log_tail } => {
                write!(f, "the process did not become healthy in time. Log tail:\n{log_tail}")
            }
        }
    }
}

/// Live processes this daemon supervises, plus the `running.json` ledger and
/// the per-key locks that make a start single-flight.
pub struct Supervisor {
    runtimes_dir: PathBuf,
    processes: Mutex<HashMap<String, Arc<RunningProcess>>>,
    start_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// How many times each process key has ever been (re)spawned by this
    /// supervisor. A fresh `RunningProcess` (new pid/port/token) is created on
    /// every spawn, so this is the only thing that survives a crash and its
    /// respawn — without it, `launches` always reads back `1` and the
    /// crash-loop signal `docs/runtime-protocol.md` §3.2 step 9 promises is
    /// dead.
    launch_counts: Mutex<HashMap<String, u32>>,
}

impl Supervisor {
    pub fn new(runtimes_dir: PathBuf) -> Supervisor {
        Supervisor {
            runtimes_dir,
            processes: Mutex::new(HashMap::new()),
            start_locks: Mutex::new(HashMap::new()),
            launch_counts: Mutex::new(HashMap::new()),
        }
    }

    fn running_json_path(&self) -> PathBuf {
        self.runtimes_dir.join("running.json")
    }

    pub fn get(&self, key: &str) -> Option<Arc<RunningProcess>> {
        self.processes.lock().unwrap().get(key).cloned()
    }

    pub fn list(&self) -> Vec<Arc<RunningProcess>> {
        self.processes.lock().unwrap().values().cloned().collect()
    }

    // `start_locks` is never pruned when a process is removed. That is a
    // deliberate trade-off, not an oversight: `keyed_lock` hands out a clone
    // of the `Arc<Mutex<()>>` for a key, and single-flight correctness
    // depends on *every* caller for that key serializing on the *same*
    // mutex instance. If an entry were removed the moment its process stops,
    // a caller who already holds a clone from before the removal, and a
    // brand-new caller who calls `keyed_lock` right after it, would end up
    // on two different `Mutex`es for the same key and could both spawn a
    // process concurrently — reintroducing the double-spawn bug single-flight
    // exists to prevent. There is no safe point to remove an entry: a caller
    // can always show up wanting to (re)start a key that was stopped
    // arbitrarily long ago. The unbounded growth this trades for is bounded
    // in practice by the number of *distinct* process keys ever started
    // (installed runtimes × loaded model files) — small for any real
    // install, unlike an actually-unbounded key space.
    fn keyed_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.start_locks.lock().unwrap();
        Arc::clone(locks.entry(key.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))))
    }

    /// Return an already-healthy process for `key`, or start one. Single-flight:
    /// concurrent callers for the same key serialize on that key's lock, and
    /// re-check for an existing process once they get it — so only the first
    /// caller actually spawns anything (§3.2 step 9).
    pub async fn ensure_started(&self, key: &str, spec: LaunchSpec) -> Result<Arc<RunningProcess>, StartError> {
        if let Some(p) = self.get(key) {
            if p.is_ready() && !self.evict_if_exited(key, &p) && !Self::outdated(&p, &spec) {
                p.touch();
                return Ok(p);
            }
        }
        let lock = self.keyed_lock(key);
        let _guard = lock.lock().await;
        if let Some(p) = self.get(key) {
            if p.is_ready() && !self.evict_if_exited(key, &p) {
                if !Self::outdated(&p, &spec) {
                    p.touch();
                    return Ok(p);
                }
                // The key names the runtime, not its version, so an update
                // that installed and selected a newer version would otherwise
                // keep being served by the old process for as long as anything
                // keeps calling it — a polled settings page means forever.
                // Swap only when idle; a request still running keeps the old
                // one, and the next call after it tries again.
                if p.in_flight.load(Ordering::SeqCst) > 0 {
                    p.touch();
                    return Ok(p);
                }
                tracing::info!(
                    "[runtime] {key}: {} {} replaces the running {}",
                    spec.manifest.id,
                    spec.manifest.version,
                    p.version
                );
                self.stop(key).await;
            }
        }
        let proc = self.spawn(key, spec).await?;
        proc.touch();
        Ok(proc)
    }

    /// Is `p` a different version of the package `spec` would launch?
    fn outdated(p: &RunningProcess, spec: &LaunchSpec) -> bool {
        p.version != spec.manifest.version
    }

    /// §3.2 step 9: a process that crashed *after* becoming healthy is
    /// otherwise invisible — `state` stays `Ready` forever since nothing
    /// polls it in the background, so every caller keeps dialing a dead port
    /// until the idle sweep eventually notices (5-15 minutes later). Called
    /// from `ensure_started`'s fast path (so the very next request recovers)
    /// and from `evict_if_crashed` (so a relay failure recovers too) — never
    /// from a timer: "the next request starts it again", not "a background
    /// loop restarts it".
    ///
    /// If `p`'s child has actually exited: marks it `Failed` (naming the exit
    /// status), removes it from the map — but only while it is still the
    /// process tracked for `key`, since a concurrent caller may already have
    /// replaced it with a fresh one — and persists. Returns whether `p` was
    /// found dead (i.e. whether the caller's `Arc` is now stale and must not
    /// be used) — not whether a map removal happened, since a second caller
    /// racing the first one may find `p` dead but `key` already pointing at
    /// someone else's fresh process by the time it checks.
    fn evict_if_exited(&self, key: &str, p: &Arc<RunningProcess>) -> bool {
        let Some(status) = p.try_wait_exited() else { return false };
        let reason = format!("the process exited unexpectedly (status: {status})");
        *p.state.lock().unwrap() = ProcessState::Failed;
        *p.error.lock().unwrap() = Some(reason.clone());
        {
            let mut processes = self.processes.lock().unwrap();
            if processes.get(key).is_some_and(|current| Arc::ptr_eq(current, p)) {
                processes.remove(key);
            }
        }
        self.persist_running_json();
        tracing::warn!("[runtime] {key} crashed: {reason}");
        true
    }

    /// After a relay to `key`'s process failed: check whether it actually
    /// crashed (rather than a transient network hiccup) and, if so, evict it
    /// so the *next* request respawns instead of dialing a dead port for the
    /// rest of the idle timeout.
    pub fn evict_if_crashed(&self, key: &str) {
        if let Some(p) = self.get(key) {
            self.evict_if_exited(key, &p);
        }
    }

    async fn spawn(&self, key: &str, spec: LaunchSpec) -> Result<Arc<RunningProcess>, StartError> {
        let port = free_port().map_err(StartError::Io)?;
        let token = random_token();

        let vars = sen_runtime_sdk::env::LaunchVars {
            id: spec.manifest.id.clone(),
            version: spec.manifest.version.clone(),
            port,
            token: token.clone(),
            data_dir: spec.data_dir.clone(),
            models_dir: spec.models_dir.clone(),
            package_dir: spec.package_dir.clone(),
            parent_pid: std::process::id(),
            config_path: spec.config_path.clone(),
            home: spec.home.clone(),
            model: spec.model.as_ref().map(|m| sen_runtime_sdk::env::ModelLaunch {
                id: m.id.clone(),
                path: m.path.clone(),
                mmproj_path: m.mmproj_path.clone(),
                context_length: m.context_length,
            }),
        };
        let placeholders = sen_runtime_sdk::env::placeholder_values(&vars);
        let capabilities = spec.model.as_ref().map(|m| m.capabilities.as_slice()).unwrap_or(&[]);
        let args = spec
            .manifest
            .launch_args(&placeholders, capabilities)
            .map_err(|e| StartError::BadManifest(e.to_string()))?;
        let manifest_env = spec
            .manifest
            .launch_env(&placeholders)
            .map_err(|e| StartError::BadManifest(e.to_string()))?;
        let command_path = spec
            .manifest
            .command_path(&spec.package_dir)
            .map_err(|e| StartError::BadManifest(e.to_string()))?;

        if let Some(parent) = spec.log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        rotate_log_if_large(&spec.log_path);
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spec.log_path)
            .map_err(|e| StartError::Io(format!("open log {}: {e}", spec.log_path.display())))?;
        let log_file_err = log_file.try_clone().map_err(|e| StartError::Io(e.to_string()))?;

        let mut cmd = tokio::process::Command::new(&command_path);
        cmd.args(&args)
            .current_dir(&spec.package_dir)
            .stdin(std::process::Stdio::null())
            .stdout(log_file)
            .stderr(log_file_err);
        for (k, v) in &manifest_env {
            cmd.env(k, v);
        }
        // The daemon's own launch env always wins over the manifest's
        // `entry.env` (docs/runtime-protocol.md §3.1).
        for (k, v) in sen_runtime_sdk::env::launch_vars(&vars) {
            cmd.env(k, v);
        }

        let child = cmd.spawn().map_err(|e| StartError::Io(format!("spawn {}: {e}", command_path.display())))?;
        let pid = child.id().unwrap_or(0);

        // §3.2 step 9: count this (re)spawn for `key`, surviving the
        // fresh `RunningProcess` a respawn always creates — the crash-loop
        // signal `launches` exists for depends on this, not on the field
        // starting over at 1 every time.
        let launches = {
            let mut counts = self.launch_counts.lock().unwrap();
            let n = counts.entry(key.to_string()).or_insert(0);
            *n += 1;
            *n
        };

        let proc = Arc::new(RunningProcess {
            key: key.to_string(),
            runtime_id: spec.manifest.id.clone(),
            version: spec.manifest.version.clone(),
            port,
            token,
            package_dir: spec.package_dir.clone(),
            started_at: now_millis(),
            last_used_at: AtomicU64::new(now_millis()),
            launches: AtomicU32::new(launches),
            in_flight: AtomicU32::new(0),
            idle_timeout_secs: spec.idle_timeout_secs,
            state: Mutex::new(ProcessState::Starting),
            error: Mutex::new(None),
            child: Mutex::new(Some(child)),
            pid,
        });
        // Track it — and write `running.json` — the moment it launches, in
        // `Starting` state, not only once the health gate passes (§3.2 step 3;
        // `docs/runtime-protocol.md` §5.1: "a process appears as `starting` as
        // soon as it is launched"). A poller must see the load in progress.
        self.processes.lock().unwrap().insert(key.to_string(), Arc::clone(&proc));
        self.persist_running_json();

        let health_path = spec.manifest.health.path.clone();
        let timeout = Duration::from_secs(spec.manifest.health.startup_timeout_secs);
        let base_url = proc.base_url();
        let health_result = wait_healthy(&proc, &base_url, &health_path, timeout).await;

        if let Err(reason) = health_result {
            let taken = proc.child.lock().unwrap().take();
            if let Some(mut child) = taken {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
            let tail = tail_of_file(&spec.log_path, 4000);
            *proc.state.lock().unwrap() = ProcessState::Failed;
            *proc.error.lock().unwrap() = Some(reason.clone());
            self.persist_running_json();
            tracing::warn!("[runtime] {} {} failed to become healthy: {reason}", spec.manifest.id, spec.manifest.version);
            return Err(StartError::Unhealthy { log_tail: tail });
        }

        *proc.state.lock().unwrap() = ProcessState::Ready;
        tracing::info!(
            "[runtime] {} {} ready on 127.0.0.1:{port} (pid {pid}, key {key})",
            proc.runtime_id,
            proc.version
        );
        Ok(proc)
    }

    /// Stop one process: `POST /runtime/shutdown` (a 404/timeout there — e.g.
    /// upstream llama.cpp, which has no such route — falls straight through to
    /// the OS signal), `SIGTERM` after 3 s, `SIGKILL` after 10 s.
    pub async fn stop(&self, key: &str) -> bool {
        let Some(proc) = self.processes.lock().unwrap().remove(key) else {
            return false;
        };
        self.persist_running_json();
        *proc.state.lock().unwrap() = ProcessState::Stopping;
        self.stop_process(&proc).await;
        true
    }

    async fn stop_process(&self, proc: &RunningProcess) {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(2)).build();
        if let Ok(client) = client {
            let url = format!("{}/runtime/shutdown", proc.base_url());
            let _ = client.post(&url).bearer_auth(&proc.token).send().await;
        }
        let Some(mut child) = proc.child.lock().unwrap().take() else { return };
        if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_ok() {
            return;
        }
        #[cfg(unix)]
        {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = child.start_kill();
        }
        if tokio::time::timeout(Duration::from_secs(10), child.wait()).await.is_err() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    /// Stop every process this daemon launched — daemon shutdown.
    pub async fn stop_all(&self) {
        let all: Vec<Arc<RunningProcess>> = self.processes.lock().unwrap().drain().map(|(_, v)| v).collect();
        self.persist_running_json();
        for proc in all {
            self.stop_process(&proc).await;
        }
    }

    /// Sweep processes idle past their timeout. `0` = never; an in-flight
    /// request always keeps the process regardless of `last_used_at`. Only a
    /// `Ready` process can be idled out — one still `Starting` has not run
    /// anything to be idle *from* yet, and a `Failed` one has no child left
    /// to stop.
    pub async fn sweep_idle(&self, default_service_secs: u64, default_model_secs: u64, is_model: impl Fn(&str) -> bool) {
        let now = now_millis();
        let candidates: Vec<Arc<RunningProcess>> = self
            .processes
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.in_flight.load(Ordering::Relaxed) == 0 && p.is_ready())
            .cloned()
            .collect();
        for proc in candidates {
            let default = if is_model(&proc.key) { default_model_secs } else { default_service_secs };
            // The manifest's own `idleTimeoutSecs` wins when the runtime
            // declared one — absent falls back to the daemon-wide default for
            // this process's mode (`docs/runtime-protocol.md` §3.1).
            let effective = proc.idle_timeout_secs.unwrap_or(default);
            if effective == 0 {
                continue;
            }
            let idle_ms = now.saturating_sub(proc.last_used_at.load(Ordering::Relaxed));
            if idle_ms >= effective * 1000 {
                tracing::info!("[runtime] idling out {} ({}s unused)", proc.key, idle_ms / 1000);
                self.stop(&proc.key).await;
            }
        }
    }

    /// At boot: stop anything left running by a previous daemon (crash,
    /// `kill -9`) whose pid is alive **and** whose working directory still
    /// matches the package it was launched from — the same verification
    /// `reclaim_app_port` uses for Space Apps, since "cannot verify" must
    /// never be treated as "yes, kill it" (§3.2 step 8).
    pub async fn cleanup_orphans(&self) {
        let path = self.running_json_path();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return;
        };
        let entries: Vec<RunningEntry> = serde_json::from_str(&raw).unwrap_or_default();
        for entry in &entries {
            if !pid_alive(entry.pid) {
                continue;
            }
            let cwd = crate::gateway::ui_server::space_mcp::process_cwd(entry.pid as i32).await;
            let want = entry.package_dir.canonicalize().unwrap_or_else(|_| entry.package_dir.clone());
            match cwd {
                Some(got) if got == want => {
                    tracing::info!(
                        "[runtime] stopping orphan {} {} (pid {}) from a previous run",
                        entry.id,
                        entry.version,
                        entry.pid
                    );
                    kill_pid(entry.pid);
                }
                _ => {
                    tracing::warn!(
                        "[runtime] pid {} is alive but its working directory could not be verified as {} \
                         — leaving it alone (stop it by hand if it is a stale runtime)",
                        entry.pid,
                        want.display()
                    );
                }
            }
        }
        // Whatever happened above, this process's own tracking starts clean —
        // any entry left alive either wasn't ours to touch or is already gone.
        self.persist_running_json();
    }

    fn persist_running_json(&self) {
        let entries: Vec<RunningEntry> = self
            .processes
            .lock()
            .unwrap()
            .values()
            .map(|p| RunningEntry {
                key: p.key.clone(),
                pid: p.pid,
                port: p.port,
                id: p.runtime_id.clone(),
                version: p.version.clone(),
                package_dir: p.package_dir.clone(),
                started_at: p.started_at,
            })
            .collect();
        if let Err(e) = write_running_json(&self.running_json_path(), &entries) {
            tracing::warn!("[runtime] could not persist running.json: {e:#}");
        }
    }
}

fn write_running_json(path: &Path, entries: &[RunningEntry]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(entries)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    listener.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return false;
        };
        let alive = WaitForSingleObject(handle, 0).0 == 258; // WAIT_TIMEOUT
        let _ = CloseHandle(handle);
        alive
    }
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn kill_pid(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
        }
    }
}

/// Rotate `<path>` to `<path>.1` when it has grown past 5 MB (§2.1: "5 MB, 1 rotation").
fn rotate_log_if_large(path: &Path) {
    const MAX_BYTES: u64 = 5 * 1024 * 1024;
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        let rotated = path.with_extension(format!(
            "{}.1",
            path.extension().and_then(|e| e.to_str()).unwrap_or("log")
        ));
        let _ = std::fs::rename(path, rotated);
    }
}

fn tail_of_file(path: &Path, max_bytes: usize) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(max_bytes);
    String::from_utf8_lossy(&bytes[start..]).to_string()
}

/// `GET {base}/{health_path}` every 250 ms until 200, or the process exits,
/// or `timeout` elapses (§3.2 step 4).
///
/// Only **500** ends the wait early: it is the runtime saying its load failed
/// for good. **503 is "still loading"** — a model-mode runtime (sen-mlx,
/// llama-server) answers it for as long as its weights take to load, so
/// treating it as a failure kills every model process a few milliseconds after
/// it starts. Anything else (connection refused, other statuses) keeps polling.
///
/// Takes the tracked `RunningProcess` rather than a bare `&mut Child`: the
/// process is already visible in `Starting` state (§3.2 step 3) by the time
/// this runs, and its `child` handle lives behind that shared value's own
/// mutex now, not a local borrow only `spawn` could see.
async fn wait_healthy(proc: &RunningProcess, base_url: &str, health_path: &str, timeout: Duration) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{base_url}{health_path}");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = proc.try_wait_exited() {
            return Err(format!("the process exited before it became healthy (status: {status})"));
        }
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) if resp.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR => {
                return Err(format!("health check answered {} (the runtime reported its load failed)", resp.status()));
            }
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("no healthy response within {}s", timeout.as_secs()));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_port_returns_something_bindable() {
        let port = free_port().unwrap();
        assert!(port > 0);
    }

    #[test]
    fn random_token_is_64_hex_chars() {
        let t = random_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, random_token(), "must not repeat");
    }

    #[test]
    fn this_process_is_alive_and_a_bogus_pid_is_not() {
        assert!(pid_alive(std::process::id()));
        // pid 1 exists on unix (init) but not necessarily reachable from a
        // sandboxed test runner; use an implausibly large pid instead.
        assert!(!pid_alive(u32::MAX - 1));
    }

    #[test]
    fn log_rotation_only_triggers_past_the_size_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("sen-ocr.log");
        std::fs::write(&log, vec![0u8; 1024]).unwrap();
        rotate_log_if_large(&log);
        assert!(log.exists(), "small file is not rotated");

        std::fs::write(&log, vec![0u8; 6 * 1024 * 1024]).unwrap();
        rotate_log_if_large(&log);
        assert!(!log.exists(), "oversized file was rotated away");
        assert!(tmp.path().join("sen-ocr.log.1").exists());
    }

    #[test]
    fn running_json_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("running.json");
        let entries = vec![RunningEntry {
            key: "service:sen-ocr".into(),
            pid: 4242,
            port: 40001,
            id: "sen-ocr".into(),
            version: "0.1.0".into(),
            package_dir: tmp.path().to_path_buf(),
            started_at: 1_700_000_000_000,
        }];
        write_running_json(&path, &entries).unwrap();
        let back: Vec<RunningEntry> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].pid, 4242);
    }

    #[tokio::test]
    async fn cleanup_orphans_leaves_running_json_empty_when_none_are_ours() {
        let tmp = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(tmp.path().to_path_buf());
        // A pid that is certainly not alive.
        let entries = vec![RunningEntry {
            key: "service:sen-ocr".into(),
            pid: u32::MAX - 1,
            port: 1,
            id: "sen-ocr".into(),
            version: "0.1.0".into(),
            package_dir: tmp.path().to_path_buf(),
            started_at: 0,
        }];
        write_running_json(&sup.running_json_path(), &entries).unwrap();
        sup.cleanup_orphans().await;
        let text = std::fs::read_to_string(sup.running_json_path()).unwrap();
        let after: Vec<RunningEntry> = serde_json::from_str(&text).unwrap();
        assert!(after.is_empty());
    }

    /// Serve `/health` answering `statuses` in order, then the last one forever.
    async fn health_server(statuses: Vec<u16>) -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(move || {
                let calls = calls.clone();
                let statuses = statuses.clone();
                async move {
                    let i = calls.fetch_add(1, Ordering::SeqCst).min(statuses.len() - 1);
                    axum::http::StatusCode::from_u16(statuses[i]).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }

    fn idle_child() -> tokio::process::Child {
        tokio::process::Command::new("sleep").arg("30").kill_on_drop(true).spawn().unwrap()
    }

    /// A minimal `RunningProcess` around a real child, for exercising
    /// `wait_healthy` directly without going through a full `spawn()`.
    fn fake_process(child: tokio::process::Child) -> RunningProcess {
        RunningProcess {
            key: "test".into(),
            runtime_id: "test".into(),
            version: "0.0.0".into(),
            port: 0,
            token: String::new(),
            package_dir: PathBuf::new(),
            started_at: now_millis(),
            last_used_at: AtomicU64::new(now_millis()),
            launches: AtomicU32::new(1),
            in_flight: AtomicU32::new(0),
            idle_timeout_secs: None,
            state: Mutex::new(ProcessState::Starting),
            error: Mutex::new(None),
            child: Mutex::new(Some(child)),
            pid: 0,
        }
    }

    fn spec_at(version: &str, dir: &Path) -> LaunchSpec {
        let manifest = serde_json::from_str(&format!(
            r#"{{"schemaVersion":1,"id":"test","name":"T","version":"{version}","type":"ocr",
              "slots":["ocr"],"capabilities":["ocr"],"platforms":["darwin-arm64"],"mode":"service",
              "entry":{{"command":"bin/missing","args":[]}} }}"#
        ))
        .unwrap();
        LaunchSpec {
            manifest,
            package_dir: dir.to_path_buf(),
            data_dir: dir.join("data"),
            models_dir: dir.join("models"),
            config_path: dir.join("config.json"),
            home: dir.to_path_buf(),
            log_path: dir.join("test.log"),
            model: None,
            idle_timeout_secs: None,
        }
    }

    /// An update installs and selects a newer version, but the process key
    /// names only the runtime: the old process must give way once idle, or a
    /// page that keeps polling it keeps the update from ever taking effect.
    #[tokio::test]
    async fn a_running_older_version_is_replaced_once_it_is_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(tmp.path().to_path_buf());
        let old = sup.track_fake_for_test("service:test"); // version 0.0.0

        let Ok(same) = sup.ensure_started("service:test", spec_at("0.0.0", tmp.path())).await else { panic!("start") };
        assert!(Arc::ptr_eq(&same, &old), "the selected version is already running");

        old.in_flight.store(1, Ordering::SeqCst);
        let Ok(busy) = sup.ensure_started("service:test", spec_at("0.0.1", tmp.path())).await else { panic!("start") };
        assert!(Arc::ptr_eq(&busy, &old), "a request in flight keeps the old process");

        old.in_flight.store(0, Ordering::SeqCst);
        // The new package has no binary here, so the respawn fails — what
        // matters is that the old process was not handed back.
        assert!(sup.ensure_started("service:test", spec_at("0.0.1", tmp.path())).await.is_err());
        assert!(sup.get("service:test").is_none_or(|p| !Arc::ptr_eq(&p, &old)));
    }

    /// A model-mode runtime answers 503 while its weights load; the gate must
    /// keep waiting through it, not kill the process on the first probe.
    #[tokio::test]
    async fn health_gate_waits_through_503_while_the_model_loads() {
        let base = health_server(vec![503, 503, 503, 200]).await;
        let proc = fake_process(idle_child());
        let result = wait_healthy(&proc, &base, "/health", Duration::from_secs(10)).await;
        assert!(result.is_ok(), "{result:?}");
    }

    /// 500 is the runtime reporting that its load failed for good — stop at
    /// once instead of waiting out the whole startup timeout.
    #[tokio::test]
    async fn health_gate_fails_fast_on_500() {
        let base = health_server(vec![503, 500]).await;
        let proc = fake_process(idle_child());
        let started = std::time::Instant::now();
        let result = wait_healthy(&proc, &base, "/health", Duration::from_secs(30)).await;
        assert!(result.unwrap_err().contains("500"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// `RunningProcess::view` composes the caller-supplied slot/model key with
    /// the tracked fields into the one shape every route uses.
    #[tokio::test]
    async fn process_view_carries_caller_supplied_slot_and_model_key() {
        let proc = fake_process(idle_child());
        *proc.state.lock().unwrap() = ProcessState::Ready;
        let v = proc.view(Some("ocr"), None);
        assert_eq!(v["slot"], "ocr");
        assert_eq!(v["modelKey"], serde_json::Value::Null);
        assert_eq!(v["state"], "ready");
        let v = proc.view(Some("gguf"), Some("gguf-model-abcd1234"));
        assert_eq!(v["modelKey"], "gguf-model-abcd1234");
    }

    /// §3.2 step 9: a process that crashes *after* becoming healthy must
    /// be detected and evicted, not left `Ready` forever with a dead child
    /// underneath — and nothing discovers this on its own, without a caller
    /// asking (no background restart loop).
    #[tokio::test]
    async fn a_process_that_exited_after_becoming_ready_is_evicted_not_left_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(tmp.path().to_path_buf());

        // A real child that exits almost immediately, standing in for a
        // runtime that crashed after its health gate passed.
        let short_lived = tokio::process::Command::new("sh").args(["-c", "exit 0"]).kill_on_drop(true).spawn().unwrap();
        let stale = Arc::new(fake_process(short_lived));
        *stale.state.lock().unwrap() = ProcessState::Ready;
        let key = "service:crash-test";
        sup.processes.lock().unwrap().insert(key.to_string(), Arc::clone(&stale));

        // Give the OS a moment to actually reap the child.
        for _ in 0..100 {
            if stale.try_wait_exited().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(stale.is_ready(), "a crash is only discovered reactively, never by a background poller");

        assert!(sup.evict_if_exited(key, &stale), "an exited child must be evicted");
        assert_eq!(*stale.state.lock().unwrap(), ProcessState::Failed);
        assert!(stale.error.lock().unwrap().as_ref().unwrap().contains("exited"), "the error must name what happened");
        assert!(sup.get(key).is_none(), "the crashed process must be removed so the next start respawns");

        // A stale `Arc` calling this again (e.g. a second concurrent caller
        // that raced the first) must never remove whatever a respawn has
        // since put at the same key — only ever its own, still-tracked entry.
        let fresh = Arc::new(fake_process(idle_child()));
        sup.processes.lock().unwrap().insert(key.to_string(), Arc::clone(&fresh));
        sup.evict_if_exited(key, &stale);
        assert!(Arc::ptr_eq(&sup.get(key).unwrap(), &fresh), "must never evict a different process that now owns this key");
    }

    #[tokio::test]
    async fn sweep_never_stops_a_process_that_is_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let sup = Supervisor::new(tmp.path().to_path_buf());
        let starting = Arc::new(fake_process(idle_child()));
        starting.last_used_at.store(0, Ordering::Relaxed); // as idle as it gets
        sup.processes.lock().unwrap().insert("service:starting".into(), Arc::clone(&starting));
        let failed = Arc::new(fake_process(idle_child()));
        *failed.state.lock().unwrap() = ProcessState::Failed;
        failed.last_used_at.store(0, Ordering::Relaxed);
        sup.processes.lock().unwrap().insert("service:failed".into(), Arc::clone(&failed));

        sup.sweep_idle(1, 1, |_| false).await;

        assert!(sup.get("service:starting").is_some(), "a still-launching process must never be idled out");
        assert!(sup.get("service:failed").is_some(), "a failed process has no child left to stop — sweep must leave it alone");
    }

    /// §3.2 step 3 / `docs/runtime-protocol.md` §5.1: "a process appears as
    /// `starting` as soon as it is launched". The manifest's script never
    /// answers `/health`, so the gate must time out — the point of the test is
    /// what is observable *during* that wait, and that the failure leaves a
    /// `failed` record behind rather than removing it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_process_is_visible_as_starting_the_moment_it_launches_and_failed_after_a_timeout() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let package_dir = tmp.path().join("pkg");
        std::fs::create_dir_all(package_dir.join("bin")).unwrap();
        std::fs::write(package_dir.join("bin").join("run"), "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(package_dir.join("bin").join("run"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let manifest_json = r#"{"schemaVersion":1,"id":"test-runtime","name":"T","version":"0.0.1","type":"ocr",
            "slots":["ocr"],"capabilities":["ocr"],
            "platforms":["darwin-arm64","darwin-x64","linux-x64","linux-arm64","windows-x64","windows-arm64"],
            "mode":"service","entry":{"command":"bin/run","args":[]},
            "health":{"path":"/health","startupTimeoutSecs":1}}"#;
        std::fs::write(package_dir.join(sen_runtime_sdk::manifest::MANIFEST_FILE), manifest_json).unwrap();
        let manifest = sen_runtime_sdk::manifest::RuntimeManifest::read_from_dir(&package_dir).unwrap().manifest;

        let sup = Arc::new(Supervisor::new(tmp.path().join("runtimes")));
        let spec = LaunchSpec {
            manifest,
            package_dir: package_dir.clone(),
            data_dir: tmp.path().join("data"),
            models_dir: tmp.path().join("models"),
            config_path: tmp.path().join("config.json"),
            home: tmp.path().to_path_buf(),
            log_path: tmp.path().join("test.log"),
            model: None,
            idle_timeout_secs: None,
        };
        let key = "service:test-runtime";
        let sup2 = Arc::clone(&sup);
        let handle = tokio::spawn(async move { sup2.ensure_started(key, spec).await });

        let mut saw_starting = false;
        for _ in 0..60 {
            if sup.get(key).is_some_and(|p| p.snapshot().state == ProcessState::Starting) {
                saw_starting = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(saw_starting, "the process must be tracked as `starting` while the health gate runs");

        let result = handle.await.unwrap();
        assert!(result.is_err(), "the script never answers /health, so this must time out");
        let after = sup.get(key).expect("a failed start stays visible as `failed`, not removed");
        assert_eq!(after.snapshot().state, ProcessState::Failed);
    }
}

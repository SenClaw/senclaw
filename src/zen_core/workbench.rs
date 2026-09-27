//! Workbench — artifact publishing surface for tools/agents.
//!
//! Port of the sema-core `ArtifactRegistry` (the `WorkbenchService` impl):
//! `code-old/SenClaw/vendor/package/dist/workbench/{ArtifactRegistry,paths}.js`.
//!
//! Three artifact modes:
//!   - **Static**: bundle of files (html/markdown) rendered by the WebUI
//!   - **Web**: external URL (e.g. running React app) embedded in iframe
//!   - **Backend**: running service with a URL + usage notes (no iframe)
//!
//! State is held per-`ZenEngine`, bound to the engine's `instance_id` +
//! `working_dir`, and mirrored to a per-instance JSON manifest on disk
//! (`<working_dir>/workbench/.artifacts.<instanceId>.json`). The
//! [`crate::agent::workbench_bridge::WorkbenchBridge`] subscribes to
//! [`EngineEvent::WorkbenchNew`] / `WorkbenchService*` events and forwards them
//! to the WebSocket gateway and IM fallbacks. The `LaunchUI` tool
//! ([`crate::tools::LaunchUITool`]) is the producer that calls
//! [`WorkbenchService::create_static`] / [`WorkbenchService::create_service`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::events::EngineEvent;
use super::EventBus;

/// Conventional fallback dir name under `working_dir` (TS default). No Rust
/// config surface exists yet, so this is hardcoded to the reference default.
const FALLBACK_DIRNAME: &str = "workbench";
/// Whether to drop fallback-dir artifact references on restart (TS default).
const CLEAR_FALLBACK_ON_RESTART: bool = true;

/// Artifact mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkbenchMode {
    /// File bundle (rendered by WebUI: HTML iframe, Markdown view, etc.).
    Static,
    /// Live web app at `url` — embedded in iframe.
    Web,
    /// Backend service — `url` + Markdown usage notes; no iframe.
    Backend,
}

impl WorkbenchMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkbenchMode::Static => "static",
            WorkbenchMode::Web => "web",
            WorkbenchMode::Backend => "backend",
        }
    }
}

/// A file inside a static artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchFile {
    /// Resolved absolute path on disk.
    pub path: String,
    /// Optional inline content. When `None`, the WebUI calls
    /// [`WorkbenchService::read_file`] to fetch from disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// sha256 of the file contents at publish time. The WebUI keys its render
    /// cache on this (see `web/src/components/workbench/StaticRenderer.tsx`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Resolved renderer kind: `"html"` | `"md"`. Inferred from the path by the
    /// WebUI when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
}

/// Service-mode process metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// "starting" | "running" | "ready" | "crashed" | "stopped"
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(rename = "lastActive", skip_serializing_if = "Option::is_none")]
    pub last_active: Option<i64>,
    #[serde(rename = "logPath", skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
}

/// A workbench artifact. Sent to UI via `workbench:new`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchArtifact {
    pub id: String,
    pub title: String,
    pub mode: WorkbenchMode,
    /// Files for `Static` mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<WorkbenchFile>,
    /// URL for `Web` / `Backend` modes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Process state (Web/Backend only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessInfo>,
    /// Markdown usage notes (Backend mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
    /// Producing agent id (manifest fidelity; ignored by the WebUI).
    #[serde(rename = "agentId", skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Owning engine instance id (manifest fidelity; ignored by the WebUI).
    #[serde(rename = "instanceId", skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Unix-millis creation timestamp.
    #[serde(rename = "createdAt")]
    pub created_at: i64,
}

/// Reason a service artifact stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Manual,
    Idle,
    SessionEnd,
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            StopReason::Manual => "manual",
            StopReason::Idle => "idle",
            StopReason::SessionEnd => "session_end",
        }
    }
}

// ============================================================================
// Event data types
// ============================================================================

/// Event: `workbench:new`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchNewData {
    pub artifact: WorkbenchArtifact,
    /// If non-None, this artifact supersedes the previously-current artifact.
    #[serde(rename = "replacesId", skip_serializing_if = "Option::is_none")]
    pub replaces_id: Option<String>,
}

/// Event: `workbench:service_ready`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchServiceReadyData {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    pub ready: bool,
}

/// Event: `workbench:service_crashed`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchServiceCrashedData {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    #[serde(rename = "lastLogLines")]
    pub last_log_lines: String,
}

/// Event: `workbench:service_stopped`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchServiceStoppedData {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    pub reason: StopReason,
}

// ============================================================================
// WorkbenchService — engine-side registry (port of ArtifactRegistry)
// ============================================================================

/// On-disk manifest shape: `{ current, items }`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    current: Option<String>,
    #[serde(default)]
    items: Vec<WorkbenchArtifact>,
}

/// Locked interior state.
#[derive(Default)]
struct Inner {
    items: HashMap<String, WorkbenchArtifact>,
    /// The currently-foregrounded artifact; older ones become history.
    current: Option<String>,
}

/// Per-engine workbench registry. Tracks live artifacts, mirrors to a
/// per-instance manifest on disk, and emits engine events on publish /
/// lifecycle changes.
pub struct WorkbenchService {
    event_bus: EventBus,
    instance_id: String,
    working_dir: PathBuf,
    inner: Mutex<Inner>,
}

impl WorkbenchService {
    /// Construct a registry bound to `instance_id` + `working_dir` and hydrate
    /// it from the on-disk manifest (if present).
    pub fn new(
        event_bus: EventBus,
        instance_id: impl Into<String>,
        working_dir: impl Into<PathBuf>,
    ) -> Self {
        let svc = Self {
            event_bus,
            instance_id: instance_id.into(),
            working_dir: working_dir.into(),
            inner: Mutex::new(Inner::default()),
        };
        svc.load_from_disk();
        svc
    }

    // ===== Factories (mirror ArtifactRegistry.createStatic / createService) ==

    /// Publish a static artifact from one or more on-disk files. Resolves each
    /// path against `working_dir`, validates existence + extension, and records
    /// a content hash. Returns `Err("path_not_found: …" | "unsupported_extension: …")`.
    pub fn create_static(
        &self,
        files: &[String],
        title: Option<String>,
        agent_id: &str,
    ) -> Result<WorkbenchArtifact, String> {
        if files.is_empty() {
            return Err("mode=static requires non-empty `files`.".to_string());
        }
        let mut resolved_files = Vec::with_capacity(files.len());
        for f in files {
            let resolved = resolve_file_path(&self.working_dir, f);
            if !resolved.exists() {
                return Err(format!("path_not_found: {f}"));
            }
            let ext = resolved
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            let kind = match ext.as_str() {
                "html" | "htm" => "html",
                "md" | "markdown" => "md",
                _ => return Err(format!("unsupported_extension: .{ext} (file: {f})")),
            };
            let hash = sha256_of_file(&resolved)?;
            resolved_files.push(WorkbenchFile {
                path: resolved.to_string_lossy().to_string(),
                content: None,
                mime_type: None,
                hash: Some(hash),
                extension: Some(kind.to_string()),
            });
        }

        let title = title.unwrap_or_else(|| default_title_from_files(&resolved_files));
        let artifact = WorkbenchArtifact {
            id: new_artifact_id(),
            title,
            mode: WorkbenchMode::Static,
            files: resolved_files,
            url: None,
            process: None,
            usage: None,
            agent_id: Some(agent_id.to_string()),
            instance_id: Some(self.instance_id.clone()),
            created_at: now_millis(),
        };
        self.commit(artifact.clone());
        Ok(artifact)
    }

    /// Publish a `web` / `backend` artifact for an already-running service. Does
    /// not start any process — the caller starts it (e.g. via Bash) and passes
    /// the reachable `url`.
    #[allow(clippy::too_many_arguments)]
    pub fn create_service(
        &self,
        mode: WorkbenchMode,
        url: String,
        title: Option<String>,
        usage: Option<String>,
        agent_id: &str,
        pid: Option<u32>,
        log_path: Option<String>,
    ) -> WorkbenchArtifact {
        let title = title.unwrap_or_else(|| url.clone());
        let process = match (pid, log_path) {
            (Some(pid), Some(lp)) => Some(ProcessInfo {
                status: "running".to_string(),
                pid: Some(pid),
                last_active: Some(now_millis()),
                log_path: Some(lp),
            }),
            _ => None,
        };
        let artifact = WorkbenchArtifact {
            id: new_artifact_id(),
            title,
            mode,
            files: Vec::new(),
            url: Some(url),
            process,
            usage,
            agent_id: Some(agent_id.to_string()),
            instance_id: Some(self.instance_id.clone()),
            created_at: now_millis(),
        };
        self.commit(artifact.clone());
        artifact
    }

    // ===== Queries =========================================================

    /// List current artifacts, newest first. Prunes artifacts whose files have
    /// disappeared.
    pub fn list(&self) -> Vec<WorkbenchArtifact> {
        self.prune_missing(None);
        let mut v: Vec<WorkbenchArtifact> =
            self.inner.lock().unwrap().items.values().cloned().collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        v
    }

    pub fn get(&self, artifact_id: &str) -> Option<WorkbenchArtifact> {
        self.prune_missing(Some(artifact_id));
        self.inner.lock().unwrap().items.get(artifact_id).cloned()
    }

    // ===== Reverse ops (called by the bridge / UI server) ==================

    /// Mark an artifact as viewed — refreshes `process.last_active` (service
    /// mode). Returns `true` when the artifact exists.
    pub fn mark_viewed(&self, artifact_id: &str) -> bool {
        let mut had_process = false;
        let exists = {
            let mut inner = self.inner.lock().unwrap();
            match inner.items.get_mut(artifact_id) {
                Some(it) => {
                    if let Some(p) = it.process.as_mut() {
                        p.last_active = Some(now_millis());
                        had_process = true;
                    }
                    true
                }
                None => false,
            }
        };
        if had_process {
            self.flush_to_disk();
        }
        exists
    }

    /// Close an artifact: drop it from the registry (and clear `current` if it
    /// pointed here). Service-mode process teardown is the caller's job.
    pub fn close(&self, artifact_id: &str) -> bool {
        let removed = {
            let mut inner = self.inner.lock().unwrap();
            let removed = inner.items.remove(artifact_id).is_some();
            if removed && inner.current.as_deref() == Some(artifact_id) {
                inner.current = None;
            }
            removed
        };
        if removed {
            self.flush_to_disk();
        }
        removed
    }

    /// Read a file belonging to a static artifact. The requested path must be in
    /// the artifact's file whitelist. Errors:
    /// `artifact_not_found` / `path_not_in_artifact` / `read_failed: …`.
    pub fn read_file(&self, artifact_id: &str, requested_path: &str) -> Result<String, String> {
        let allowed: Vec<String> = {
            let inner = self.inner.lock().unwrap();
            match inner.items.get(artifact_id) {
                Some(it) => it.files.iter().map(|f| f.path.clone()).collect(),
                None => return Err("artifact_not_found".to_string()),
            }
        };
        let resolved = resolve_file_path(&self.working_dir, requested_path)
            .to_string_lossy()
            .to_string();
        if !allowed.iter().any(|p| p == &resolved) {
            return Err("path_not_in_artifact".to_string());
        }
        match std::fs::read_to_string(&resolved) {
            Ok(content) => Ok(content),
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    self.prune_missing(Some(artifact_id));
                }
                Err(format!("read_failed: {e}"))
            }
        }
    }

    /// Tail the artifact's log file (service mode). Returns "" when no log.
    pub fn fetch_logs(&self, artifact_id: &str, tail_lines: usize) -> String {
        let log_path: Option<String> = self
            .inner
            .lock()
            .unwrap()
            .items
            .get(artifact_id)
            .and_then(|a| a.process.as_ref()?.log_path.clone());
        let Some(path) = log_path else {
            return String::new();
        };
        match std::fs::read_to_string(Path::new(&path)) {
            Ok(s) => {
                let lines: Vec<&str> = s.lines().collect();
                let start = lines.len().saturating_sub(tail_lines.max(1));
                lines[start..].join("\n")
            }
            Err(_) => String::new(),
        }
    }

    // ===== Service-mode lifecycle notifiers (called by process-owning tools) =

    pub fn notify_service_ready(&self, artifact_id: &str) {
        self.set_process_status(artifact_id, "ready");
        self.event_bus.emit(EngineEvent::WorkbenchServiceReady(
            WorkbenchServiceReadyData {
                artifact_id: artifact_id.to_string(),
                ready: true,
            },
        ));
    }

    pub fn notify_service_crashed(&self, artifact_id: &str, last_log_lines: String) {
        self.set_process_status(artifact_id, "crashed");
        self.event_bus.emit(EngineEvent::WorkbenchServiceCrashed(
            WorkbenchServiceCrashedData {
                artifact_id: artifact_id.to_string(),
                last_log_lines,
            },
        ));
    }

    pub fn notify_service_stopped(&self, artifact_id: &str, reason: StopReason) {
        self.set_process_status(artifact_id, "stopped");
        self.event_bus.emit(EngineEvent::WorkbenchServiceStopped(
            WorkbenchServiceStoppedData {
                artifact_id: artifact_id.to_string(),
                reason,
            },
        ));
    }

    /// Stop all live services on session end. Idempotent.
    pub fn shutdown(&self) {
        let ids: Vec<String> = self
            .inner
            .lock()
            .unwrap()
            .items
            .values()
            .filter(|a| a.process.is_some())
            .map(|a| a.id.clone())
            .collect();
        for id in ids {
            self.notify_service_stopped(&id, StopReason::SessionEnd);
        }
    }

    // ===== Internal ========================================================

    /// Insert + promote to `current`, persist, and emit `workbench:new`. The
    /// previously-current artifact is retained (history), not deleted.
    fn commit(&self, artifact: WorkbenchArtifact) {
        let previous = {
            let mut inner = self.inner.lock().unwrap();
            let previous = inner.current.clone();
            inner.items.insert(artifact.id.clone(), artifact.clone());
            inner.current = Some(artifact.id.clone());
            previous
        };
        self.flush_to_disk();
        self.event_bus
            .emit(EngineEvent::WorkbenchNew(WorkbenchNewData {
                artifact,
                replaces_id: previous,
            }));
    }

    fn set_process_status(&self, artifact_id: &str, status: &str) {
        let changed = {
            let mut inner = self.inner.lock().unwrap();
            match inner.items.get_mut(artifact_id) {
                Some(a) => match a.process.as_mut() {
                    Some(p) => {
                        p.status = status.to_string();
                        true
                    }
                    None => false,
                },
                None => false,
            }
        };
        if changed {
            self.flush_to_disk();
        }
    }

    /// Remove static artifacts whose referenced files no longer exist. With
    /// `only_id`, validates just that artifact (lazy check from `get`).
    fn prune_missing(&self, only_id: Option<&str>) {
        let mut changed = false;
        {
            let mut inner = self.inner.lock().unwrap();
            let targets: Vec<String> = match only_id {
                Some(id) => {
                    if inner.items.contains_key(id) {
                        vec![id.to_string()]
                    } else {
                        vec![]
                    }
                }
                None => inner.items.keys().cloned().collect(),
            };
            for id in targets {
                let missing = match inner.items.get(&id) {
                    Some(it) if !it.files.is_empty() => {
                        it.files.iter().any(|f| !Path::new(&f.path).exists())
                    }
                    _ => false,
                };
                if missing {
                    inner.items.remove(&id);
                    if inner.current.as_deref() == Some(id.as_str()) {
                        inner.current = None;
                    }
                    changed = true;
                    tracing::info!("[Workbench] pruned artifact {id} (file missing)");
                }
            }
        }
        if changed {
            self.flush_to_disk();
        }
    }

    fn flush_to_disk(&self) {
        let manifest = {
            let inner = self.inner.lock().unwrap();
            Manifest {
                current: inner.current.clone(),
                items: inner.items.values().cloned().collect(),
            }
        };
        if let Err(e) = std::fs::create_dir_all(self.instance_dir()) {
            tracing::error!("[Workbench] ensure dir failed: {e}");
            return;
        }
        match serde_json::to_string_pretty(&manifest) {
            Ok(json) => {
                if let Err(e) = std::fs::write(self.manifest_path(), json) {
                    tracing::error!("[Workbench] flush manifest failed: {e}");
                }
            }
            Err(e) => tracing::error!("[Workbench] serialize manifest failed: {e}"),
        }
    }

    fn load_from_disk(&self) {
        let path = self.manifest_path();
        if !path.exists() {
            return;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("[Workbench] read manifest failed: {e}");
                return;
            }
        };
        let manifest: Manifest = match serde_json::from_str(&raw) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("[Workbench] load manifest failed: {e}");
                return;
            }
        };

        let fallback_prefix = format!(
            "{}{}",
            self.instance_dir().to_string_lossy(),
            std::path::MAIN_SEPARATOR
        );
        {
            let mut inner = self.inner.lock().unwrap();
            for it in manifest.items {
                if CLEAR_FALLBACK_ON_RESTART {
                    // Drop "transient display links" living under the fallback
                    // dir; physical-file GC is a separate concern.
                    let in_fallback = !it.files.is_empty()
                        && it
                            .files
                            .iter()
                            .any(|f| f.path.starts_with(&fallback_prefix));
                    if in_fallback {
                        continue;
                    }
                }
                inner.items.insert(it.id.clone(), it);
            }
            inner.current = match manifest.current {
                Some(c) if inner.items.contains_key(&c) => Some(c),
                _ => None,
            };
        }
        // The user may have moved/deleted files between sessions.
        self.prune_missing(None);
        let count = self.inner.lock().unwrap().items.len();
        tracing::info!(
            "[Workbench] loaded {count} artifacts for instance {}",
            self.instance_id
        );
    }

    // ---- path helpers (port of paths.js) ----

    fn workbench_root(&self) -> PathBuf {
        self.working_dir.join(FALLBACK_DIRNAME)
    }

    fn instance_dir(&self) -> PathBuf {
        self.workbench_root().join(&self.instance_id)
    }

    fn manifest_path(&self) -> PathBuf {
        self.workbench_root()
            .join(format!(".artifacts.{}.json", self.instance_id))
    }
}

// ===== free helpers =========================================================

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_artifact_id() -> String {
    format!("wb_{}", &uuid::Uuid::new_v4().simple().to_string()[..10])
}

/// Resolve a user/agent-supplied path: absolute as-is, relative against `wd`.
fn resolve_file_path(working_dir: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    }
}

fn sha256_of_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read_failed: {e}"))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn default_title_from_files(files: &[WorkbenchFile]) -> String {
    let first = files
        .first()
        .map(|f| {
            Path::new(&f.path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| f.path.clone())
        })
        .unwrap_or_default();
    if files.len() > 1 {
        format!("{first} (+{})", files.len() - 1)
    } else {
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("wb-test-{}", uuid::Uuid::new_v4()))
    }

    fn write_file(dir: &Path, name: &str, content: &str) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p.to_string_lossy().to_string()
    }

    fn svc_in(dir: &Path) -> WorkbenchService {
        WorkbenchService::new(EventBus::new(), "inst1", dir.to_path_buf())
    }

    #[test]
    fn create_static_resolves_hash_and_extension() {
        let dir = tmp_dir();
        let f = write_file(&dir, "report.md", "# hi");
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let svc = WorkbenchService::new(bus, "inst1", dir.clone());

        let a = svc.create_static(&[f.clone()], None, "agent-1").unwrap();
        assert_eq!(a.mode, WorkbenchMode::Static);
        assert_eq!(a.title, "report.md"); // default title = basename
        assert_eq!(a.files.len(), 1);
        assert_eq!(a.files[0].extension.as_deref(), Some("md"));
        assert!(a.files[0].hash.as_ref().unwrap().len() == 64);
        assert_eq!(a.agent_id.as_deref(), Some("agent-1"));
        assert!(matches!(
            rx.try_recv().unwrap(),
            EngineEvent::WorkbenchNew(_)
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_static_rejects_unsupported_extension() {
        let dir = tmp_dir();
        let f = write_file(&dir, "data.txt", "nope");
        let svc = svc_in(&dir);
        let err = svc.create_static(&[f], None, "a").unwrap_err();
        assert!(err.starts_with("unsupported_extension"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_static_rejects_missing_file() {
        let dir = tmp_dir();
        let svc = svc_in(&dir);
        let err = svc
            .create_static(
                &[dir.join("ghost.md").to_string_lossy().to_string()],
                None,
                "a",
            )
            .unwrap_err();
        assert!(err.starts_with("path_not_found"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_sets_replaces_id_to_previous_current() {
        let dir = tmp_dir();
        let f1 = write_file(&dir, "a.md", "a");
        let f2 = write_file(&dir, "b.md", "b");
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let svc = WorkbenchService::new(bus, "inst1", dir.clone());

        let a = svc.create_static(&[f1], None, "x").unwrap();
        let _b = svc.create_static(&[f2], None, "x").unwrap();
        // both retained (history)
        assert_eq!(svc.list().len(), 2);

        // first event has no replaces; second replaces the first
        let ev1 = rx.try_recv().unwrap();
        let ev2 = rx.try_recv().unwrap();
        match (ev1, ev2) {
            (EngineEvent::WorkbenchNew(d1), EngineEvent::WorkbenchNew(d2)) => {
                assert!(d1.replaces_id.is_none());
                assert_eq!(d2.replaces_id.as_deref(), Some(a.id.as_str()));
            }
            _ => panic!("expected two WorkbenchNew events"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_whitelist() {
        let dir = tmp_dir();
        let f = write_file(&dir, "page.html", "<h1>hi</h1>");
        let svc = svc_in(&dir);
        let a = svc.create_static(&[f.clone()], None, "x").unwrap();

        assert_eq!(svc.read_file(&a.id, &f).unwrap(), "<h1>hi</h1>");
        assert_eq!(
            svc.read_file(&a.id, "/etc/passwd").unwrap_err(),
            "path_not_in_artifact"
        );
        assert_eq!(svc.read_file("nope", &f).unwrap_err(), "artifact_not_found");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn manifest_round_trips() {
        let dir = tmp_dir();
        let f = write_file(&dir, "doc.md", "x");
        let id;
        {
            let svc = svc_in(&dir);
            id = svc.create_static(&[f], None, "x").unwrap().id;
            assert!(svc.manifest_path().exists());
        }
        // fresh instance hydrates from disk
        let svc2 = svc_in(&dir);
        assert!(svc2.get(&id).is_some());
        assert_eq!(svc2.list().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prune_missing_drops_deleted_files() {
        let dir = tmp_dir();
        let f = write_file(&dir, "tmp.md", "x");
        let svc = svc_in(&dir);
        let a = svc.create_static(&[f.clone()], None, "x").unwrap();
        assert!(svc.get(&a.id).is_some());

        std::fs::remove_file(&f).unwrap();
        assert!(svc.get(&a.id).is_none()); // lazy prune in get()
        assert_eq!(svc.list().len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_service_web() {
        let dir = tmp_dir();
        let svc = svc_in(&dir);
        let a = svc.create_service(
            WorkbenchMode::Web,
            "http://localhost:5173".to_string(),
            None,
            None,
            "x",
            None,
            None,
        );
        assert_eq!(a.mode, WorkbenchMode::Web);
        assert_eq!(a.title, "http://localhost:5173"); // default title = url
        assert_eq!(a.url.as_deref(), Some("http://localhost:5173"));
        assert!(a.process.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn close_removes_and_clears_current() {
        let dir = tmp_dir();
        let f = write_file(&dir, "c.md", "x");
        let svc = svc_in(&dir);
        let a = svc.create_static(&[f], None, "x").unwrap();
        assert!(svc.close(&a.id));
        assert!(!svc.close(&a.id));
        assert!(svc.get(&a.id).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shutdown_only_stops_service_artifacts() {
        let dir = tmp_dir();
        let f = write_file(&dir, "s.md", "x");
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let svc = WorkbenchService::new(bus, "inst1", dir.clone());

        svc.create_static(&[f], None, "x").unwrap();
        svc.create_service(
            WorkbenchMode::Web,
            "http://localhost:1234".to_string(),
            None,
            None,
            "x",
            Some(4242),
            Some("/tmp/log.txt".to_string()),
        );
        // drain publish events
        while rx.try_recv().is_ok() {}

        svc.shutdown();
        let mut stops = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, EngineEvent::WorkbenchServiceStopped(_)) {
                stops += 1;
            }
        }
        assert_eq!(stops, 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}

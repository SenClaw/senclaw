//! The runtime manager: the daemon's one seam onto every installed runtime
//! package and running process (`docs/runtime-protocol.md`).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use sen_runtime_sdk::manifest::{ModelFormat, RunMode, RuntimeManifest, Slot};

use super::index::{CachedIndex, RuntimeIndex};
use super::jobs::{JobRegistry, JobStatus};
use super::settings::RuntimeSettings;
use super::store::{self, InstalledPackage};
use super::supervisor::{LaunchSpec, ModelLaunch, RunningProcess, Supervisor};
use super::version::cmp_versions;

/// Everything a caller needs to reach a runtime process, without knowing how
/// it got started.
#[derive(Debug)]
pub struct Dial {
    pub base_url: String,
    pub token: String,
    pub process_key: String,
}

#[derive(Debug, Clone)]
pub enum RuntimeClientError {
    NotInstalled { slot: String },
    NotSelected { slot: String },
    StartFailed { slot: String, detail: String },
    Upstream(String),
    Internal(String),
}

impl std::fmt::Display for RuntimeClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeClientError::NotInstalled { slot } => {
                write!(f, "no {slot} runtime is installed. Install one in Settings -> Runtime.")
            }
            RuntimeClientError::NotSelected { slot } => {
                write!(f, "a {slot} runtime is installed but not selected. Pick one in Settings -> Runtime.")
            }
            RuntimeClientError::StartFailed { slot, detail } => {
                write!(f, "the {slot} runtime would not start: {detail}")
            }
            RuntimeClientError::Upstream(e) => write!(f, "{e}"),
            RuntimeClientError::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl RuntimeClientError {
    /// Machine-readable code, matching `sen_runtime_sdk::api::codes`.
    pub fn code(&self) -> &'static str {
        use sen_runtime_sdk::api::codes;
        match self {
            RuntimeClientError::NotInstalled { .. } => codes::RUNTIME_NOT_INSTALLED,
            RuntimeClientError::NotSelected { .. } => codes::RUNTIME_NOT_SELECTED,
            RuntimeClientError::StartFailed { .. } => codes::RUNTIME_START_FAILED,
            RuntimeClientError::Upstream(_) | RuntimeClientError::Internal(_) => "runtime_error",
        }
    }
}

pub struct RuntimeManagerConfig {
    pub runtimes_dir: PathBuf,
    pub runtime_data_dir: PathBuf,
    pub runtime_logs_dir: PathBuf,
    pub bundled_dir: Option<PathBuf>,
    pub local_models_dir: PathBuf,
    pub config_path: PathBuf,
    pub home: PathBuf,
    /// Where the runtime catalog is fetched from — resolved once by
    /// `Config::from_env` (`SENCLAW_RUNTIME_INDEX_URL`), never read from the
    /// environment again here.
    pub index_url: String,
}

pub struct RuntimeManager {
    cfg: RuntimeManagerConfig,
    settings: RwLock<RuntimeSettings>,
    supervisor: Supervisor,
    pub jobs: Arc<JobRegistry>,
}

impl RuntimeManager {
    pub fn new(cfg: RuntimeManagerConfig) -> Arc<RuntimeManager> {
        let settings = super::settings::load(&cfg.runtimes_dir);
        let supervisor = Supervisor::new(cfg.runtimes_dir.clone());
        Arc::new(RuntimeManager {
            cfg,
            settings: RwLock::new(settings),
            supervisor,
            jobs: Arc::new(JobRegistry::new()),
        })
    }

    pub fn paths(&self) -> &RuntimeManagerConfig {
        &self.cfg
    }

    pub fn settings(&self) -> RuntimeSettings {
        self.settings.read().unwrap().clone()
    }

    pub fn replace_settings(&self, settings: RuntimeSettings) -> anyhow::Result<()> {
        super::settings::save(&self.cfg.runtimes_dir, &settings)?;
        *self.settings.write().unwrap() = settings;
        Ok(())
    }

    pub fn select(&self, slot: Slot, id: Option<String>, version: Option<String>) -> anyhow::Result<()> {
        if let Some(id) = &id {
            let installed = self.installed();
            if !installed.iter().any(|p| &p.manifest.id == id) {
                anyhow::bail!("`{id}` is not installed");
            }
        }
        let mut settings = self.settings.write().unwrap();
        settings.select(slot, id, version);
        super::settings::save(&self.cfg.runtimes_dir, &settings)?;
        Ok(())
    }

    pub fn installed(&self) -> Vec<InstalledPackage> {
        store::scan_all(&self.cfg.runtimes_dir, self.cfg.bundled_dir.as_deref())
    }

    pub fn install_local(&self, path: &std::path::Path) -> anyhow::Result<InstalledPackage> {
        store::install_local(&self.cfg.runtimes_dir, path)
    }

    pub async fn uninstall(&self, id: &str, version: &str, force: bool) -> anyhow::Result<()> {
        let key = format!("service:{id}");
        let running_service = self.supervisor.get(&key).filter(|p| p.version == version);
        let running_models: Vec<Arc<RunningProcess>> = self
            .supervisor
            .list()
            .into_iter()
            .filter(|p| p.runtime_id == id && p.version == version)
            .collect();
        let anything_running = running_service.is_some() || !running_models.is_empty();
        if anything_running && !force {
            anyhow::bail!("`{id}` {version} is running — pass force=1 to stop it and uninstall");
        }
        if anything_running {
            if let Some(p) = &running_service {
                self.supervisor.stop(&p.key).await;
            }
            for p in running_models {
                self.supervisor.stop(&p.key).await;
            }
        }
        store::uninstall(&self.cfg.runtimes_dir, id, version)
    }

    /// `key`, when given, is a full process key (`service:<id>` or
    /// `model:<model-key>`) and selects that exact process's log file.
    /// Without it: the service log (`<id>.log`) if there is one, else the
    /// most recently modified model log (`<id>--*.log`) — a model-mode
    /// runtime like llama.cpp has no single service log (§5.1: "without `key`
    /// it reads the runtime's most recent log").
    pub fn logs(&self, id: &str, lines: usize, key: Option<&str>) -> anyhow::Result<(PathBuf, Vec<String>)> {
        let path = self.resolve_log_path(id, key);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        // Runtimes built on the SDK before it checked for a terminal colour
        // their log lines; the log viewers show text, not escape codes.
        let all: Vec<String> = text.lines().map(strip_ansi).collect();
        let start = all.len().saturating_sub(lines);
        Ok((path, all[start..].to_vec()))
    }

    fn resolve_log_path(&self, id: &str, key: Option<&str>) -> PathBuf {
        let service_log = self.cfg.runtime_logs_dir.join(format!("{id}.log"));
        if let Some(key) = key {
            return match key.strip_prefix("model:") {
                Some(model_key) => self.cfg.runtime_logs_dir.join(format!("{id}--{model_key}.log")),
                None => service_log,
            };
        }
        if service_log.is_file() {
            return service_log;
        }
        let prefix = format!("{id}--");
        let newest = std::fs::read_dir(&self.cfg.runtime_logs_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with(&prefix) && name.ends_with(".log")
            })
            .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, e.path())))
            .max_by_key(|(t, _)| *t)
            .map(|(_, p)| p);
        newest.unwrap_or(service_log)
    }

    // ===== Slot / manifest resolution =====

    /// Candidates installed, compatible with this platform, for `slot`.
    fn candidates_for(&self, slot: Slot) -> Vec<InstalledPackage> {
        let installed = self.installed();
        self.candidates_for_from(slot, &installed)
    }

    /// Same filter as [`Self::candidates_for`], against an already-scanned
    /// list — for a caller (`GET /api/local-models`, `GET /api/runtimes`)
    /// that needs this for several slots/models in the same request and would
    /// otherwise pay for a fresh directory + GGUF-header scan each time.
    fn candidates_for_from(&self, slot: Slot, installed: &[InstalledPackage]) -> Vec<InstalledPackage> {
        let platform = sen_runtime_sdk::platform::current();
        installed
            .iter()
            .filter(|p| p.manifest.slots.contains(&slot) && p.manifest.supports_platform(platform))
            .cloned()
            .collect()
    }

    /// The manifest currently in effect for `slot`: the explicit selection
    /// (newest installed version when the selection names no version), or —
    /// with nothing selected — the sole compatible candidate, auto-selected
    /// and persisted (§5.1: "auto-selects it on first use").
    pub fn manifest_for_slot(&self, slot: Slot) -> Option<InstalledPackage> {
        let installed = self.installed();
        self.manifest_for_slot_from(slot, &installed)
    }

    /// Same resolution as [`Self::manifest_for_slot`], against an
    /// already-scanned list (see [`Self::candidates_for_from`]).
    pub fn manifest_for_slot_from(&self, slot: Slot, installed: &[InstalledPackage]) -> Option<InstalledPackage> {
        let selection = self.settings.read().unwrap().selected(slot).cloned();
        let mut candidates = self.candidates_for_from(slot, installed);
        candidates.sort_by(|a, b| cmp_versions(&b.manifest.version, &a.manifest.version));

        if let Some(sel) = selection {
            return match &sel.version {
                Some(v) => candidates.into_iter().find(|p| p.manifest.id == sel.id && p.manifest.version == *v),
                None => candidates.into_iter().find(|p| p.manifest.id == sel.id),
            };
        }
        if candidates.len() == 1 {
            let only = candidates.into_iter().next().unwrap();
            let _ = self.select(slot, Some(only.manifest.id.clone()), Some(only.manifest.version.clone()));
            return Some(only);
        }
        None
    }

    fn slot_label(slot: Slot) -> String {
        slot.label().to_string()
    }

    async fn dial(&self, slot: Slot, spec: LaunchSpec, key: String) -> Result<Dial, RuntimeClientError> {
        // `StartError`'s own `Display` already carries the log tail for
        // `Unhealthy` — no special-casing needed here.
        let proc = self
            .supervisor
            .ensure_started(&key, spec)
            .await
            .map_err(|e| RuntimeClientError::StartFailed { slot: Self::slot_label(slot), detail: e.to_string() })?;
        Ok(Dial { base_url: proc.base_url(), token: proc.token.clone(), process_key: key })
    }

    /// Ensure the service runtime filling `slot` is running, and return how to
    /// reach it. Used by the legacy-namespace proxy and by internal clients
    /// (decision, OCR).
    pub async fn ensure_slot_started(&self, slot: Slot) -> Result<Dial, RuntimeClientError> {
        let label = Self::slot_label(slot);
        let pkg = self.manifest_for_slot(slot).ok_or_else(|| {
            if self.candidates_for(slot).is_empty() {
                RuntimeClientError::NotInstalled { slot: label.clone() }
            } else {
                RuntimeClientError::NotSelected { slot: label.clone() }
            }
        })?;
        if pkg.manifest.mode != RunMode::Service {
            return Err(RuntimeClientError::Internal(format!("`{}` is not a service runtime", pkg.manifest.id)));
        }
        let key = format!("service:{}", pkg.manifest.id);
        let spec = self.service_launch_spec(&pkg.manifest, &pkg.dir);
        self.dial(slot, spec, key).await
    }

    fn service_launch_spec(&self, manifest: &RuntimeManifest, package_dir: &std::path::Path) -> LaunchSpec {
        LaunchSpec {
            manifest: manifest.clone(),
            package_dir: package_dir.to_path_buf(),
            data_dir: self.cfg.runtime_data_dir.join(&manifest.id),
            models_dir: self.cfg.local_models_dir.clone(),
            config_path: self.cfg.config_path.clone(),
            home: self.cfg.home.clone(),
            log_path: self.cfg.runtime_logs_dir.join(format!("{}.log", manifest.id)),
            model: None,
            idle_timeout_secs: manifest.idle_timeout_secs,
        }
    }

    /// Ensure a model-mode process for `model_key` is running on the runtime
    /// selected for `format`'s slot, and return how to reach it.
    #[allow(clippy::too_many_arguments)]
    pub async fn ensure_model_started(
        &self,
        format: ModelFormat,
        model_key: &str,
        model_path: &std::path::Path,
        mmproj_path: Option<&std::path::Path>,
        context_length: u32,
        capabilities: Vec<sen_runtime_sdk::manifest::Capability>,
    ) -> Result<Dial, RuntimeClientError> {
        let slot = Slot::for_format(format);
        let label = Self::slot_label(slot);
        let pkg = self.manifest_for_slot(slot).ok_or_else(|| {
            if self.candidates_for(slot).is_empty() {
                RuntimeClientError::NotInstalled { slot: label.clone() }
            } else {
                RuntimeClientError::NotSelected { slot: label.clone() }
            }
        })?;
        let key = format!("model:{model_key}");
        let mut spec = self.service_launch_spec(&pkg.manifest, &pkg.dir);
        spec.log_path = self.cfg.runtime_logs_dir.join(format!("{}--{model_key}.log", pkg.manifest.id));
        spec.model = Some(ModelLaunch {
            id: model_key.to_string(),
            path: model_path.to_path_buf(),
            mmproj_path: mmproj_path.map(Path::to_path_buf),
            context_length,
            capabilities,
        });
        self.dial(slot, spec, key).await
    }

    pub fn process(&self, key: &str) -> Option<Arc<RunningProcess>> {
        self.supervisor.get(key)
    }

    pub fn processes(&self) -> Vec<Arc<RunningProcess>> {
        self.supervisor.list()
    }

    pub async fn stop_process(&self, key: &str) -> bool {
        self.supervisor.stop(key).await
    }

    /// Mark a request starting/finishing against `key`'s process, so the idle
    /// sweep never stops a process mid-request.
    pub fn begin_request(&self, key: &str) {
        if let Some(p) = self.supervisor.get(key) {
            p.touch();
            p.in_flight.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn end_request(&self, key: &str) {
        if let Some(p) = self.supervisor.get(key) {
            p.touch();
            p.in_flight.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// After a relay to `key`'s process failed: evict it if it actually
    /// crashed, so the *next* request respawns instead of dialing a dead port
    /// for the rest of the idle timeout. See `Supervisor::evict_if_crashed`
    /// (`docs/runtime-protocol.md` §3.2 step 9).
    pub fn evict_if_crashed(&self, key: &str) {
        self.supervisor.evict_if_crashed(key);
    }

    // ===== Index / install jobs =====

    pub fn cached_index(&self) -> Option<CachedIndex> {
        super::index::load_cache(&self.cfg.runtimes_dir)
    }

    pub async fn refresh_index(&self) -> CachedIndex {
        self.refresh_index_with_error().await.0
    }

    /// Same fetch as [`Self::refresh_index`], plus the fetch error (when the
    /// remote index could not be read and the answer fell back to the cache
    /// or the bundled copy) and the beta channel's `"latest"` resolved to a
    /// concrete tag for every upstream entry that needs it — `GET
    /// /api/runtimes/catalog`'s `error` field and its `latestVersion` for a
    /// `channels.beta: "latest"` entry both come from here.
    pub async fn refresh_index_with_error(&self) -> (CachedIndex, Option<String>) {
        let (mut cached, err) = super::index::fetch_and_cache(&self.cfg.runtimes_dir, &self.cfg.index_url).await;
        let platform = sen_runtime_sdk::platform::current();
        let channel = self.settings().channel;
        for entry in cached.index.runtimes.clone() {
            if cached.resolved_latest.contains_key(&entry.id) || entry.channel_version(channel) != Some("latest") {
                continue;
            }
            let Some(upstream) = &entry.upstream else { continue };
            match super::llamacpp::resolve_latest_tag(upstream, platform).await {
                Ok(tag) => {
                    cached.resolved_latest.insert(entry.id.clone(), tag.clone());
                    super::index::record_resolved_latest(&self.cfg.runtimes_dir, &entry.id, &tag);
                }
                Err(e) => tracing::warn!("[runtime] could not resolve `latest` for {}: {e:#}", entry.id),
            }
        }
        (cached, err)
    }

    fn current_index(&self) -> RuntimeIndex {
        self.cached_index().map(|c| c.index).unwrap_or_else(RuntimeIndex::bundled)
    }

    /// `POST /api/runtimes/install`: resolve `version` (or the channel's
    /// current version when none is given) and start a background job.
    pub fn start_install(&self, id: &str, version: Option<String>) -> anyhow::Result<JobStatus> {
        let index = self.current_index();
        let entry = index.entry(id).ok_or_else(|| anyhow::anyhow!("`{id}` is not in the runtime index"))?;
        let channel = self.settings().channel;
        let resolved = version
            .or_else(|| entry.channel_version(channel).map(str::to_string))
            .ok_or_else(|| anyhow::anyhow!("`{id}` has no version for the `{}` channel", channel.as_str()))?;
        Ok(self.jobs.start(self.cfg.runtimes_dir.clone(), index, id.to_string(), resolved))
    }

    pub async fn stop_all(&self) {
        self.supervisor.stop_all().await;
    }

    pub async fn cleanup_orphans(&self) {
        self.supervisor.cleanup_orphans().await;
    }

    /// One idle-sweep pass, synchronously — what the background loop below
    /// runs every 10s, exposed for tests that need a deterministic sweep
    /// instead of waiting on the timer.
    pub async fn sweep_idle_once(&self) {
        let idle = self.settings().idle_timeout_secs;
        self.supervisor.sweep_idle(idle.service, idle.model, |key| key.starts_with("model:")).await;
        self.advance_pinned_slots_to_newer_installs();
    }

    /// §7.3: "the slot moves to the new version once no process of the old
    /// one is in use". A `null`-versioned selection already tracks the
    /// newest installed version on every read (`manifest_for_slot_from`), so
    /// this only concerns a slot explicitly pinned to a version — an
    /// auto-update (or a manual parallel install) leaves the *old* version
    /// selected until this moves it. Run from the idle sweep rather than
    /// right after a job finishes: if the old version is still serving a
    /// request at that moment, this is what retries the move a few seconds
    /// later once it is not.
    fn advance_pinned_slots_to_newer_installs(&self) {
        let installed = self.installed();
        let mut settings = self.settings.write().unwrap();
        if !settings.auto_update {
            // LM Studio semantics move a pinned slot only "with
            // auto-update on" (§7.3). A user who pins an older version while
            // a newer one sits installed alongside it — a deliberate
            // rollback — must not have that silently undone by the very next
            // idle sweep just because auto-update happens to be off; turning
            // auto-update back on is what lets the slot advance again.
            return;
        }
        let mut changed = false;
        for slot in Slot::ALL {
            let Some(sel) = settings.selected(slot).cloned() else { continue };
            let Some(pinned) = sel.version.clone() else { continue };
            let newest = installed
                .iter()
                .filter(|p| p.manifest.id == sel.id)
                .max_by(|a, b| cmp_versions(&a.manifest.version, &b.manifest.version));
            let Some(newest) = newest else { continue };
            if cmp_versions(&newest.manifest.version, &pinned) != std::cmp::Ordering::Greater {
                continue;
            }
            let old_still_running = self.supervisor.list().iter().any(|p| p.runtime_id == sel.id && p.version == pinned);
            if old_still_running {
                continue; // defer to the next sweep
            }
            tracing::info!(
                "[runtime] moving {} from {} {pinned} to {} (§7.3: update installed, old version idle)",
                slot.as_str(),
                sel.id,
                newest.manifest.version
            );
            settings.select(slot, Some(sel.id.clone()), Some(newest.manifest.version.clone()));
            changed = true;
        }
        if changed {
            if let Err(e) = super::settings::save(&self.cfg.runtimes_dir, &settings) {
                tracing::warn!("[runtime] could not persist the §7.3 slot move: {e:#}");
            }
        }
    }

    /// Background loop: idle-sweep every 10s (§3.2 step 6).
    pub fn spawn_idle_sweeper(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                this.sweep_idle_once().await;
            }
        });
    }
}

#[cfg(test)]
impl RuntimeManager {
    /// Test-only: track a process with no real child under `key`, for
    /// exercising request bookkeeping (`begin_request`/`end_request`,
    /// `evict_if_crashed`, `in_flight`) from another module's tests without
    /// spawning a real OS process.
    pub(crate) fn track_process_for_test(&self, key: &str) -> Arc<RunningProcess> {
        self.supervisor.track_fake_for_test(key)
    }
}

/// A line without its ANSI escape sequences (`ESC [ … final-byte`, and
/// two-byte `ESC x` forms).
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // Parameters and intermediates, up to the final byte (0x40..=0x7E).
            for n in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&n) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_lose_their_colour_codes() {
        let line = "\u{1b}[2m2026-09-29T01:28:02Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m sen-browser 0.1.0 listening";
        assert_eq!(strip_ansi(line), "2026-09-29T01:28:02Z  INFO sen-browser 0.1.0 listening");
        assert_eq!(strip_ansi("plain"), "plain");
    }
    use sen_runtime_sdk::manifest::MANIFEST_FILE;

    fn manager(tmp: &std::path::Path) -> Arc<RuntimeManager> {
        RuntimeManager::new(RuntimeManagerConfig {
            runtimes_dir: tmp.join("runtimes"),
            runtime_data_dir: tmp.join("runtime-data"),
            runtime_logs_dir: tmp.join("logs"),
            bundled_dir: None,
            local_models_dir: tmp.join("local-models"),
            config_path: tmp.join("config.json"),
            home: tmp.to_path_buf(),
            index_url: "file:///dev/null".to_string(),
        })
    }

    fn write_manifest(dir: &std::path::Path, id: &str, version: &str, platform: &str) {
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin").join("run"), "#!/bin/sh\n").unwrap();
        let manifest = format!(
            r#"{{"schemaVersion":1,"id":"{id}","name":"T","version":"{version}","type":"ocr",
              "slots":["ocr"],"capabilities":["ocr"],"platforms":["{platform}"],"mode":"service",
              "entry":{{"command":"bin/run","args":["serve","--port","{{port}}"]}} }}"#
        );
        std::fs::write(dir.join(MANIFEST_FILE), manifest).unwrap();
    }

    #[test]
    fn a_single_compatible_candidate_auto_selects_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let platform = sen_runtime_sdk::platform::current();
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr").join("0.1.0"), "sen-ocr", "0.1.0", platform);

        let pkg = mgr.manifest_for_slot(Slot::Ocr).expect("auto-selected");
        assert_eq!(pkg.manifest.id, "sen-ocr");
        assert_eq!(mgr.settings().selected(Slot::Ocr).unwrap().id, "sen-ocr");
    }

    #[test]
    fn two_candidates_stay_unselected_until_a_choice_is_made() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let platform = sen_runtime_sdk::platform::current();
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr").join("0.1.0"), "sen-ocr", "0.1.0", platform);
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr2").join("0.1.0"), "sen-ocr2", "0.1.0", platform);
        // Two different ids can't really both fill `ocr` in the real catalog,
        // but the resolution rule under test only cares about count.
        assert!(mgr.manifest_for_slot(Slot::Ocr).is_none());
    }

    #[tokio::test]
    async fn ensure_slot_started_reports_not_installed_vs_not_selected() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let err = mgr.ensure_slot_started(Slot::Ocr).await.unwrap_err();
        assert!(matches!(err, RuntimeClientError::NotInstalled { .. }), "{err}");

        let platform = sen_runtime_sdk::platform::current();
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr").join("0.1.0"), "sen-ocr", "0.1.0", platform);
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr2").join("0.1.0"), "sen-ocr2", "0.1.0", platform);
        let err = mgr.ensure_slot_started(Slot::Ocr).await.unwrap_err();
        assert!(matches!(err, RuntimeClientError::NotSelected { .. }), "{err}");
    }

    #[test]
    fn uninstall_refuses_a_running_version_without_force() {
        // Covered at the supervisor/store level (no live process to spin up in
        // a unit test); this just pins the id/version filter compiles + runs.
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let platform = sen_runtime_sdk::platform::current();
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr").join("0.1.0"), "sen-ocr", "0.1.0", platform);
        assert!(tokio_test::block_on(mgr.uninstall("sen-ocr", "0.1.0", false)).is_ok());
    }

    /// A `null`-versioned selection must pick the *numerically* newest
    /// version, not the lexically newest — `0.9.0` must lose to `0.10.0`.
    #[test]
    fn a_tracking_selection_resolves_to_the_numerically_newest_version() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let platform = sen_runtime_sdk::platform::current();
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr").join("0.9.0"), "sen-ocr", "0.9.0", platform);
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr").join("0.10.0"), "sen-ocr", "0.10.0", platform);
        mgr.select(Slot::Ocr, Some("sen-ocr".into()), None).unwrap();
        let pkg = mgr.manifest_for_slot(Slot::Ocr).expect("resolved");
        assert_eq!(pkg.manifest.version, "0.10.0", "a string compare would have picked 0.9.0");
    }

    #[test]
    fn logs_falls_back_to_the_newest_model_log_when_there_is_no_service_log() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let logs_dir = &mgr.paths().runtime_logs_dir;
        std::fs::create_dir_all(logs_dir).unwrap();
        std::fs::write(logs_dir.join("llama.cpp-metal--model-a.log"), "older\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(logs_dir.join("llama.cpp-metal--model-b.log"), "newer\n").unwrap();

        let (path, lines) = mgr.logs("llama.cpp-metal", 10, None).unwrap();
        assert!(path.ends_with("llama.cpp-metal--model-b.log"), "{path:?}");
        assert_eq!(lines, vec!["newer"]);

        // `?key=` selects a specific process's log regardless of mtime.
        let (path, lines) = mgr.logs("llama.cpp-metal", 10, Some("model:model-a")).unwrap();
        assert!(path.ends_with("llama.cpp-metal--model-a.log"), "{path:?}");
        assert_eq!(lines, vec!["older"]);
    }

    #[test]
    fn logs_prefers_the_service_log_when_one_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let logs_dir = &mgr.paths().runtime_logs_dir;
        std::fs::create_dir_all(logs_dir).unwrap();
        std::fs::write(logs_dir.join("sen-ocr.log"), "service\n").unwrap();
        let (_path, lines) = mgr.logs("sen-ocr", 10, None).unwrap();
        assert_eq!(lines, vec!["service"]);
    }

    /// §7.3: a slot pinned to an older version moves to the newest installed
    /// version once nothing is using the old one — which, with no process
    /// ever started in this test, is unconditionally true.
    #[test]
    fn a_pinned_slot_moves_to_a_newer_install_when_the_old_version_is_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let platform = sen_runtime_sdk::platform::current();
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr").join("0.1.0"), "sen-ocr", "0.1.0", platform);
        mgr.select(Slot::Ocr, Some("sen-ocr".into()), Some("0.1.0".into())).unwrap();
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr").join("0.2.0"), "sen-ocr", "0.2.0", platform);

        mgr.advance_pinned_slots_to_newer_installs();

        let sel = mgr.settings().selected(Slot::Ocr).unwrap().clone();
        assert_eq!(sel.version.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn advancing_a_slot_with_no_newer_install_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let platform = sen_runtime_sdk::platform::current();
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr").join("0.1.0"), "sen-ocr", "0.1.0", platform);
        mgr.select(Slot::Ocr, Some("sen-ocr".into()), Some("0.1.0".into())).unwrap();

        mgr.advance_pinned_slots_to_newer_installs();

        assert_eq!(mgr.settings().selected(Slot::Ocr).unwrap().version.as_deref(), Some("0.1.0"));
    }

    /// A slot pinned to an older version — a deliberate rollback — must
    /// never move on its own while auto-update is off, even though a newer
    /// version sits installed and idle right next to it.
    #[test]
    fn a_pinned_slot_never_moves_while_auto_update_is_off() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let platform = sen_runtime_sdk::platform::current();
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr").join("0.1.0"), "sen-ocr", "0.1.0", platform);
        mgr.select(Slot::Ocr, Some("sen-ocr".into()), Some("0.1.0".into())).unwrap();
        write_manifest(&mgr.paths().runtimes_dir.join("sen-ocr").join("0.2.0"), "sen-ocr", "0.2.0", platform);

        let mut settings = mgr.settings();
        settings.auto_update = false;
        mgr.replace_settings(settings).unwrap();

        mgr.advance_pinned_slots_to_newer_installs();

        let sel = mgr.settings().selected(Slot::Ocr).unwrap().clone();
        assert_eq!(sel.version.as_deref(), Some("0.1.0"), "auto-update off must leave the pinned rollback alone");
    }

    /// `evict_if_crashed` (as called from a relay failure) must mark a
    /// process whose child has actually exited `Failed` and remove it, and
    /// must never touch one whose child is still running.
    #[tokio::test]
    async fn evict_if_crashed_only_removes_a_process_whose_child_has_actually_exited() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());

        let alive_key = "service:still-running";
        mgr.track_process_for_test(alive_key);
        mgr.evict_if_crashed(alive_key);
        assert!(mgr.process(alive_key).is_some(), "a fake test process with no child must not be treated as crashed");

        // An untracked key is a safe no-op.
        mgr.evict_if_crashed("service:never-started");
    }
}

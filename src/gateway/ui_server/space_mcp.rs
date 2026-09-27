//! Space App runtime: launch "server" apps and auto-register their MCP.
//!
//! A Space App manifest may declare a `runtime.kind == "server"` block with a
//! `start` command (e.g. `npm start`). On install and on daemon startup,
//! SenClaw will:
//!   1. launch the app's start command from its install directory with an
//!      assigned `PORT` (so one process serves the UI + `/mcp` route + API),
//!   2. wait for the app's health endpoint,
//!   3. record the running origin into the stored manifest (`runtime.url`) so
//!      the Web UI iframe loads it,
//!   4. auto-register the declared MCP (`mcp.autoRegister`) pointing at the
//!      running origin (`mcp.url` or origin + `mcp.path`).
//!
//! The launched process is tracked per app and killed (whole process group) on
//! daemon shutdown. Legacy apps that declare only an `mcp` block with an
//! absolute `url` (no server runtime) are still auto-registered without launch.
//!
//! # Two lifecycles
//!
//! Every server app is one of two things, declared as `runtime.mode`
//! ([`crate::apps::RunMode`]):
//!
//! - **background** — started with SenClaw, supervised, restarted when it dies.
//!   For apps that do work nobody asked for at that moment: polling a channel
//!   for inbound messages, running a schedule, holding the WebSocket a browser
//!   extension dials into. Stopping one of these loses messages.
//! - **session** (the default) — started when it is *used* and stopped once it
//!   has been idle for `runtime.idleTimeoutSecs` (60s by default). "Used" means
//!   a request reached it through the daemon's app proxy: the user opened its
//!   screen, or an agent called one of its MCP tools.
//!
//! Session was made the default because the previous behaviour was to launch
//! every installed app at boot and keep it forever — on a machine with fifty
//! installed apps, fifty resident servers, nearly all of them idle.
//!
//! The trick that makes on-demand MCP work is where a session app's MCP server
//! is *pointed*: not at the app's own port (nothing is listening there), but at
//! `/api/space/apps/<id>/proxy<mcp.path>` on the daemon, which starts the app
//! before forwarding. Its tool list comes from a cache written at the last
//! successful connection, so the tools are in the agent's roster while the app
//! is stopped — otherwise nothing would ever call one, and it would never
//! start.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rusqlite::params;
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::apps::manifest::{Requires, RunMode, RuntimeSpec};
use crate::db::Db;
use crate::mcp::config::{ExternalMcpServerConfig, McpScopeType, McpToolDef, McpTransportType};
use crate::mcp::manager::McpManager;

struct ChildProc {
    child: Child,
    /// Process-group id (== leader pid) so we can signal the whole tree.
    pgid: i32,
    port: u16,
    log_path: PathBuf,
    /// The app's allowlisting egress proxy, when it runs sandboxed with
    /// `network: hosts`. Held here so it lives exactly as long as the process it
    /// was opened for: dropping this record stops the proxy.
    proxy: Option<std::sync::Arc<crate::sandbox::proxy::HostProxy>>,
    /// Wall-clock start, so the monitor can say "up 4m" — and, more usefully,
    /// "up 6s" for an app that is crash-looping while the row still reads
    /// "running".
    started_at: std::time::SystemTime,
    /// Whether this launch was confined, and how. Recorded at spawn because the
    /// stored settings can be edited afterwards, and what is *running* is what
    /// the monitor must report.
    isolation: String,
    /// Background or session — recorded at spawn for the same reason as
    /// `isolation`: the manifest can change while the process runs.
    mode: RunMode,
    /// How long this app may sit unused before the reaper stops it. Session
    /// apps only.
    idle_timeout: Duration,
    /// Last time a request reached this app through the daemon's proxy — the
    /// user's screen, or an agent's MCP call. What the idle reaper measures.
    last_activity: std::time::Instant,
}

/// What the monitor knows about one app's process, taken under the lock and
/// handed back as a plain value so nothing borrows the launcher's map.
pub struct RuntimeInfo {
    pub pid: u32,
    pub pgid: i32,
    pub port: u16,
    pub log_path: PathBuf,
    pub started_at: std::time::SystemTime,
    pub isolation: String,
    pub proxy: Option<(u16, crate::sandbox::proxy::ProxyStats)>,
    /// `background` or `session`.
    pub mode: &'static str,
    /// Seconds since the last request reached this app. For a session app,
    /// this counting past `idle_timeout_secs` is what stops it.
    pub idle_secs: u64,
    /// 0 for a background app — it is never stopped for being idle.
    pub idle_timeout_secs: u64,
}

/// Tracks server-app processes launched on behalf of Space Apps, keyed by app id.
pub struct SpaceMcpLauncher {
    children: Mutex<HashMap<String, ChildProc>>,
    /// How many times each app has been (re)launched this daemon run. A number
    /// that keeps climbing on its own is the signature of a crash loop, which
    /// otherwise looks exactly like a healthy app in every other view.
    launches: Mutex<HashMap<String, u32>>,
    /// Per-app spawn lock so concurrent callers (proxy lazy-spawn + supervisor +
    /// user restart) never double-launch the same app.
    start_locks: Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>>,
    /// Apps served by a process this daemon did not start and could not take
    /// back. Remembered so the supervisor attempts the reclaim once per daemon
    /// run instead of once every tick — the answer will not change by itself.
    adopted: Mutex<std::collections::HashSet<String>>,
    /// Apps the user explicitly stopped. The supervisor leaves these alone —
    /// otherwise pressing Stop on a background app would put it straight back
    /// up within one supervisor tick, which reads as the button not working.
    /// Cleared by an explicit start, and by anything that uses the app.
    user_stopped: Mutex<std::collections::HashSet<String>>,
    http: reqwest::Client,
    /// Space-App API contract version stamped into every app's environment
    /// (`SENCLAW_API_VERSION`). Carried here because the launcher is the one
    /// place that builds an app's environment and it holds no `Config`.
    api_version: u32,
}

impl Default for SpaceMcpLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl SpaceMcpLauncher {
    pub fn new() -> Self {
        Self::with_api_version(crate::apps::token::API_VERSION)
    }

    /// The daemon passes `config.space_api_version`, which an operator may pin
    /// with `SENCLAW_API_VERSION` while debugging an app against an older
    /// contract.
    pub fn with_api_version(api_version: u32) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            children: Mutex::new(HashMap::new()),
            launches: Mutex::new(HashMap::new()),
            start_locks: Mutex::new(HashMap::new()),
            adopted: Mutex::new(std::collections::HashSet::new()),
            user_stopped: Mutex::new(std::collections::HashSet::new()),
            http,
            api_version,
        }
    }

    /// Record that something just used this app, so the idle reaper starts
    /// counting again from now. Called by the app proxy — which is the one path
    /// every use travels: the UI iframe, the app's own REST calls, and (for a
    /// session app) every MCP tool call.
    pub async fn touch(&self, app_id: &str) {
        if let Some(proc) = self.children.lock().await.get_mut(app_id) {
            proc.last_activity = std::time::Instant::now();
        }
        self.user_stopped.lock().await.remove(app_id);
    }

    /// Is this app's process tracked and alive?
    pub async fn is_running(&self, app_id: &str) -> bool {
        self.children
            .lock()
            .await
            .get_mut(app_id)
            .map(|p| matches!(p.child.try_wait(), Ok(None)))
            .unwrap_or(false)
    }

    /// Is this app **answering** on `port`, right now?
    ///
    /// Distinct from [`is_running`], which only says a process is tracked and
    /// has not exited. A UI that loads an app's page on "tracked" renders white
    /// for as long as the process takes to bind and serve — and renders white
    /// forever if the port is held by an orphan from a previous daemon run
    /// rather than by the process we think it is.
    pub async fn is_answering(&self, port: u16, health_path: &str) -> bool {
        self.is_healthy(&health_url(port, health_path)).await
    }

    /// Wait until the app answers, or give up. Returns whether it answered.
    ///
    /// Same cadence as the post-spawn wait, so a caller that starts an app and
    /// a caller that merely probes one agree on what "up" means.
    pub async fn wait_answering(&self, port: u16, health_path: &str, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.is_answering(port, health_path).await {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Did the user stop this app by hand?
    pub async fn is_user_stopped(&self, app_id: &str) -> bool {
        self.user_stopped.lock().await.contains(app_id)
    }

    /// Stop an app at the user's request: kill the process group, drop its MCP
    /// connection, and remember the decision so the supervisor does not undo it.
    ///
    /// The MCP registration itself stays. Its tools remain in every agent's
    /// roster from the cached list, and a call reconnects through the proxy —
    /// which starts the app again. Un-registering instead would make "stop"
    /// mean "this app's tools vanish until you restart the daemon".
    pub async fn stop_by_user(&self, app_id: &str, manager: Option<&McpManager>, mcp_name: Option<&str>) {
        self.user_stopped.lock().await.insert(app_id.to_string());
        self.stop_app(app_id).await;
        if let (Some(mgr), Some(name)) = (manager, mcp_name) {
            let _ = mgr.disconnect_server(name).await;
        }
    }

    /// Get-or-create the per-app spawn lock.
    async fn start_lock(&self, app_id: &str) -> std::sync::Arc<Mutex<()>> {
        self.start_locks
            .lock()
            .await
            .entry(app_id.to_string())
            .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
            .clone()
    }

    /// Fully stop the tracked process for an app: signal the whole process
    /// group (SIGTERM → SIGKILL) and reap it. No-op if not tracked. Used by the
    /// supervisor to drop a dead child before respawning.
    pub async fn restart_app(&self, app_id: &str) {
        let tracked = self.children.lock().await.remove(app_id);
        if let Some(proc) = tracked {
            tracing::info!("[space-mcp] killing process group for app {}", app_id);
            kill_child_group(proc).await;
        }
    }

    /// Full user-facing restart: kill the tracked process group, reclaim the
    /// port (including orphans from a previous botched restart that still hold
    /// it), wait for it to free, then respawn + re-register. Starts the app even
    /// if it wasn't running. Returns the registered MCP name, if any.
    pub async fn restart_and_respawn(
        &self,
        db: &Db,
        manager: &McpManager,
        app_id: &str,
        app_dir: &Path,
        manifest: &Value,
        base_url: &str,
    ) -> Result<Option<String>> {
        // Serialize against proxy lazy-spawns / supervisor for this app.
        let lock = self.start_lock(app_id).await;
        let _guard = lock.lock().await;

        // 1. Kill the tracked child's whole group and reap it.
        let mut ports_to_free: Vec<u16> = Vec::new();
        if let Some(proc) = self.children.lock().await.remove(app_id) {
            ports_to_free.push(proc.port);
            kill_child_group(proc).await;
        }
        // 2. Also reclaim the last persisted port — this is where an orphaned
        //    grandchild (npm/next-server) from an earlier kill still listens.
        if let Some(p) = manifest
            .get("runtime")
            .and_then(|r| r.get("port"))
            .and_then(Value::as_u64)
        {
            let p = p as u16;
            if p > 0 && !ports_to_free.contains(&p) {
                ports_to_free.push(p);
            }
        }
        for p in ports_to_free {
            kill_port_listeners(p).await;
            // Wait (≤2s) until the port is actually free before respawning.
            for _ in 0..20 {
                if port_is_free(p) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        // 3. Respawn + persist the running origin + re-register the MCP.
        self.run_and_register(db, manager, app_id, app_dir, manifest, base_url)
            .await
    }

    /// Ensure the app is up and healthy, spawning it if necessary, and return
    /// the live port. Persists the running origin so the proxy targets the right
    /// process. Serialized per app to avoid double-spawns from concurrent proxy
    /// requests. Intended for the proxy's lazy self-heal path.
    pub async fn ensure_running(
        &self,
        db: &Db,
        app_id: &str,
        app_dir: &Path,
        manifest: &Value,
        base_url: &str,
    ) -> Result<u16> {
        let spec = RuntimeSpec::parse(manifest);
        if !spec.is_server {
            return Err(anyhow!("app '{app_id}' has no server runtime to start"));
        }
        let lock = self.start_lock(app_id).await;
        let _guard = lock.lock().await;

        let requires = Requires::parse(manifest);
        let port = self
            .ensure_server_running(db, app_id, app_dir, &spec, &requires, base_url)
            .await?;
        // Whoever asked for this is about to use it.
        self.touch(app_id).await;
        // Persist current origin/port so subsequent proxy hits go to this process.
        let mut m = manifest.clone();
        if let Some(rt) = m.get_mut("runtime").and_then(|v| v.as_object_mut()) {
            rt.insert(
                "url".into(),
                Value::String(format!("http://127.0.0.1:{port}")),
            );
            rt.insert("port".into(), Value::from(port));
        }
        update_app_manifest(db, app_id, &m);
        Ok(port)
    }

    /// Boot pass over every enabled installed Space App.
    ///
    /// Background apps are launched and their MCP registered against the live
    /// process, as before. Session apps are **not** launched: their MCP is
    /// registered against the daemon's app proxy with the tool list cached from
    /// the last time they ran, so their tools are available to agents while
    /// they sit stopped. Best-effort throughout — one broken app must not stop
    /// the pass.
    pub async fn autoregister_installed(
        &self,
        db: &Db,
        manager: &McpManager,
        apps_dir: &Path,
        base_url: &str,
    ) {
        let apps = enabled_apps(db);
        let (mut bg, mut session) = (0usize, 0usize);

        for (app_id, manifest) in apps {
            let app_dir = app_install_dir(&manifest, apps_dir, &app_id);
            // The app's own sandbox declaration is re-applied on every boot, so
            // a `force`d confinement survives someone editing the engine DB and
            // an app update that tightens it takes effect without a reinstall.
            crate::apps::sandbox_decl::apply(&app_id, &manifest);

            // From the cache, without starting anything. An app that is about to
            // be launched below re-registers from its live `/v1/models`; doing it
            // here first is what covers the two paths that never reach
            // `run_and_register` — a session app whose MCP came from cache, and
            // an app that declares an `llm` block and no `mcp` block at all.
            self.register_llm(db, &app_id, &app_dir, &manifest, base_url, false)
                .await;

            let spec = RuntimeSpec::parse(&manifest);
            if spec.is_server && !spec.mode.is_background() {
                session += 1;
                // An app whose tools we already know is registered without ever
                // being started. One we have never run has to be started once,
                // to learn them — a roster entry with no tools is the same as
                // no entry, and nothing would ever call it. The reaper stops it
                // a minute later, so this costs one launch per app, ever.
                match self
                    .register_session_mcp(db, manager, &app_id, &app_dir, &manifest, base_url)
                    .await
                {
                    Ok(Some(name)) => {
                        tracing::info!(
                            "[space-mcp] '{app_id}' is a session app — registered '{name}' on \
                             demand, not launched"
                        );
                        continue;
                    }
                    // Not an MCP app at all: nothing to register, nothing to start.
                    Ok(None) => continue,
                    Err(e) => tracing::info!(
                        "[space-mcp] session app '{app_id}': {e} — starting it once to learn its \
                         tools"
                    ),
                }
            } else if spec.is_server {
                bg += 1;
            }
            match self
                .run_and_register(db, manager, &app_id, &app_dir, &manifest, base_url)
                .await
            {
                Ok(Some(name)) => {
                    tracing::info!("[space-mcp] auto-registered '{name}' for app '{app_id}'")
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("[space-mcp] auto-register for app '{app_id}' failed: {e}")
                }
            }
        }
        tracing::info!(
            "[space-mcp] boot pass: {bg} background app(s) launched, {session} session app(s) \
             registered on demand"
        );
    }

    /// Register this app as an LLM provider, if it declares one.
    ///
    /// `live` says whether the app is running *now*: if it is, its `/v1/models`
    /// is asked and the answer cached, so a later boot can register the same
    /// models without starting anything. A stopped app is registered from that
    /// cache — which is the whole reason it exists. Without it a session app's
    /// models would be absent from the picker while it is stopped, so nobody
    /// would select one, so nothing would ever call the app, so it would never
    /// start and never populate the cache.
    ///
    /// Best-effort: an app that cannot be registered as a provider is still a
    /// perfectly good app, and taking its screen and its MCP tools down over it
    /// would be a worse outcome than a missing model.
    async fn register_llm(
        &self,
        db: &Db,
        app_id: &str,
        app_dir: &Path,
        manifest: &Value,
        base_url: &str,
        live: bool,
    ) {
        use crate::apps::llm_provider::{self, AppProvider};

        let decl = match crate::apps::manifest::LlmDecl::parse(manifest) {
            Ok(Some(d)) => d,
            Ok(None) => return,
            // Loud, and naming the field: these are the spellings that would
            // otherwise register a provider which fails at turn time with an
            // error mentioning neither the app nor the manifest.
            Err(e) => {
                tracing::warn!("[app-llm] '{app_id}': invalid `llm` block — {e}");
                return;
            }
        };
        if !decl.auto_register {
            return;
        }

        // Always the proxy, even for a background app with a port of its own.
        // See `crate::apps::llm_provider` — a recorded port is stale after a
        // restart and may be held by an orphan, while the proxy resolves the
        // live process, starts a stopped one, and marks the app as in use so the
        // idle reaper does not stop it mid-conversation.
        let endpoint = format!("{}{}", session_mcp_origin(base_url, app_id), decl.path);

        let mut models = Vec::new();
        if live {
            match llm_provider::fetch_models(&self.http, &endpoint).await {
                Ok(m) if !m.is_empty() => {
                    write_models_cache(app_dir, &m);
                    models = m;
                }
                Ok(_) => tracing::warn!("[app-llm] '{app_id}': /models returned nothing"),
                Err(e) => tracing::warn!("[app-llm] '{app_id}': /models failed ({e})"),
            }
        }
        if models.is_empty() {
            models = llm_provider::read_models_cache(app_dir);
        }
        if models.is_empty() {
            tracing::info!(
                "[app-llm] '{app_id}' declares an llm block but no models are known yet — it will \
                 register the first time it runs"
            );
            return;
        }

        let label = decl
            .display_name
            .clone()
            .or_else(|| {
                manifest
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| app_id.to_string());

        let provider = AppProvider {
            app_id: app_id.to_string(),
            label,
            adapt: decl.adapt.clone(),
            base_url: endpoint,
            models,
            edit_format: decl.edit_format,
        };
        let count = provider.models.len();
        match llm_provider::register(db, &provider) {
            Ok(()) => tracing::info!("[app-llm] '{app_id}': registered {count} model(s)"),
            Err(e) => tracing::warn!("[app-llm] '{app_id}': registration failed ({e})"),
        }
    }

    /// Register a session app's MCP without starting it.
    ///
    /// The URL points at the daemon's own app proxy rather than the app's port,
    /// because the app's port has nothing behind it: the proxy is what starts
    /// the process on the first request. The tools come from the cache written
    /// at the last successful connection — an app that has never run has none,
    /// and is launched instead, once, to learn them.
    async fn register_session_mcp(
        &self,
        db: &Db,
        manager: &McpManager,
        app_id: &str,
        app_dir: &Path,
        manifest: &Value,
        base_url: &str,
    ) -> Result<Option<String>> {
        let mcp = match manifest.get("mcp") {
            Some(v) if v.is_object() => v.clone(),
            _ => return Ok(None),
        };
        if !mcp
            .get("autoRegister")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(None);
        }
        let name = mcp_server_name(&mcp, app_id);
        let cached = read_tool_cache(app_dir);
        if cached.is_empty() {
            // Nothing is known about this app's tools, and a roster entry with
            // no tools is the same as no entry at all. Start it once, learn
            // them, cache them; the idle reaper stops it a minute later.
            return Err(anyhow!(
                "no cached tool list yet — it will be learned the first time the app runs"
            ));
        }
        let origin = session_mcp_origin(base_url, app_id);
        let mut config = build_mcp_config(&name, &mcp, app_id, base_url, Some(&origin), true)?;
        stamp_app_identity(&mut config, db, app_id, self.api_version);
        manager
            .add_or_update_offline(config, McpScopeType::Project, cached)
            .await
            .with_context(|| format!("register MCP '{name}' on demand"))?;
        Ok(Some(name))
    }

    /// Stop every session app that has been idle longer than its timeout.
    ///
    /// This is the other half of on-demand: without it, the first MCP call of
    /// the day would start an app that then stays up forever, which is the
    /// behaviour we replaced. Background apps are never touched.
    pub async fn reap_idle(&self, db: &Db, manager: &McpManager, apps_dir: &Path) {
        let now = std::time::Instant::now();
        let expired: Vec<(String, Duration)> = {
            let mut children = self.children.lock().await;
            let mut out = Vec::new();
            for (id, proc) in children.iter_mut() {
                let idle = now.duration_since(proc.last_activity);
                // A process that has already exited is the supervisor's problem,
                // not the reaper's.
                if !proc.mode.is_background()
                    && idle >= proc.idle_timeout
                    && matches!(proc.child.try_wait(), Ok(None))
                {
                    out.push((id.clone(), idle));
                }
            }
            out
        };
        if expired.is_empty() {
            return;
        }
        let manifests: HashMap<String, Value> = enabled_apps(db).into_iter().collect();
        for (app_id, idle) in expired {
            // Serialize against a spawn racing us — otherwise a request that
            // arrives in this instant starts an app we are about to kill.
            let lock = self.start_lock(&app_id).await;
            let _guard = lock.lock().await;
            let still_idle = {
                let children = self.children.lock().await;
                children
                    .get(&app_id)
                    .map(|p| std::time::Instant::now().duration_since(p.last_activity) >= p.idle_timeout)
                    .unwrap_or(false)
            };
            if !still_idle {
                continue;
            }
            tracing::info!(
                "[space-mcp] '{app_id}' idle for {}s → stopping (session app)",
                idle.as_secs()
            );
            self.stop_app(&app_id).await;
            // Drop the MCP connection. The registration stays — it already
            // points at the daemon's app proxy (a session app's always does),
            // so the next tool call reconnects through it and starts the app.
            if let Some(manifest) = manifests.get(&app_id) {
                if let Some(mcp) = manifest.get("mcp").filter(|v| v.is_object()) {
                    let _ = manager.disconnect_server(&mcp_server_name(mcp, &app_id)).await;
                }
            }
        }
        let _ = apps_dir;
    }

    /// Health-check every enabled **background** server app and respawn any
    /// that is down or stopped responding. Called on an interval by the
    /// daemon's Space-App supervisor loop — this is what keeps a crashed/killed
    /// app (or one that served a broken deploy) automatically coming back.
    ///
    /// Session apps are skipped entirely. For them "not running" is the resting
    /// state, not a fault: respawning one would undo both the idle reaper and
    /// the user's own Stop, and would put every installed app back to
    /// always-on within one tick.
    pub async fn supervise(&self, db: &Db, manager: &McpManager, apps_dir: &Path, base_url: &str) {
        let apps = enabled_apps(db);

        for (app_id, manifest) in apps {
            let spec = RuntimeSpec::parse(&manifest);
            if !spec.is_server || !spec.mode.is_background() {
                continue;
            }
            if self.user_stopped.lock().await.contains(&app_id) {
                continue;
            }
            let port = spec.port;
            let health_path = spec.health_path.as_str();

            let tracked = {
                let mut children = self.children.lock().await;
                children
                    .get_mut(&app_id)
                    .map(|p| matches!(p.child.try_wait(), Ok(None)))
                    .unwrap_or(false)
            };
            // Healthy if the fixed port answers its health endpoint; for a
            // dynamic port, if the tracked child is still alive.
            let healthy = if port > 0 {
                self.is_healthy(&health_url(port, health_path)).await
            } else {
                tracked
            };
            // A port that answers with nothing tracked behind it is a process
            // from a previous daemon. It serves fine, which is exactly why it
            // used to go unnoticed for weeks — running old code, from an old
            // directory, outside whatever sandbox the settings now ask for. So
            // it is treated as work to do, once: `ensure_server_running` takes
            // the port back when the process is verifiably this app's, and
            // `adopted` stops us retrying every tick when it is not.
            let untracked_stranger =
                healthy && !tracked && !self.adopted.lock().await.contains(&app_id);
            if healthy && !untracked_stranger {
                continue;
            }

            if untracked_stranger {
                tracing::warn!(
                    "[space-mcp] supervisor: '{app_id}' is served by a process this daemon did \
                     not start → reclaiming"
                );
            } else {
                tracing::warn!("[space-mcp] supervisor: app '{app_id}' is DOWN → respawning");
            }
            // Serialize with proxy lazy-spawns / user restarts for this app.
            let lock = self.start_lock(&app_id).await;
            let _guard = lock.lock().await;
            // Drop any dead tracked child so ensure_server_running spawns fresh.
            self.restart_app(&app_id).await;
            let app_dir = app_install_dir(&manifest, apps_dir, &app_id);
            match self
                .run_and_register(db, manager, &app_id, &app_dir, &manifest, base_url)
                .await
            {
                Ok(_) => tracing::info!("[space-mcp] supervisor: respawned '{app_id}'"),
                Err(e) => tracing::warn!("[space-mcp] supervisor: respawn '{app_id}' failed: {e}"),
            }
        }
    }

    /// Launch (if a server runtime) and auto-register a single app's MCP.
    /// Updates the stored manifest with the running origin. Returns the
    /// registered MCP server name, or `None` when nothing to register.
    pub async fn run_and_register(
        &self,
        db: &Db,
        manager: &McpManager,
        app_id: &str,
        app_dir: &Path,
        manifest: &Value,
        base_url: &str,
    ) -> Result<Option<String>> {
        let mut manifest = manifest.clone();
        let spec = RuntimeSpec::parse(&manifest);

        // Launch a server runtime, if declared, and record the running origin.
        let origin = if spec.is_server {
            let port = self
                .ensure_server_running(
                    db,
                    app_id,
                    app_dir,
                    &spec,
                    &Requires::parse(&manifest),
                    base_url,
                )
                .await
                .with_context(|| format!("launch server app '{app_id}'"))?;
            let origin = format!("http://127.0.0.1:{port}");
            // Persist the running origin so the iframe + detail page can reach it.
            if let Some(rt) = manifest.get_mut("runtime").and_then(|v| v.as_object_mut()) {
                rt.insert("url".into(), Value::String(origin.clone()));
                rt.insert("port".into(), Value::from(port));
            }
            update_app_manifest(db, app_id, &manifest);
            Some(origin)
        } else {
            None
        };
        // A session app's MCP is addressed through the daemon's app proxy, not
        // its own port: the port is empty whenever the app is stopped, which is
        // most of the time, and the proxy is what starts it. Registering the
        // live port instead would work exactly until the first idle timeout.
        let session = spec.is_server && !spec.mode.is_background();
        let mcp_origin = if session {
            Some(session_mcp_origin(base_url, app_id))
        } else {
            origin.clone()
        };

        // Before the `mcp` early-returns below: an app may serve models and no
        // MCP tools at all, and it must still be registered.
        self.register_llm(db, app_id, app_dir, &manifest, base_url, origin.is_some())
            .await;

        // Auto-register the MCP server, if declared.
        let mcp = match manifest.get("mcp") {
            Some(v) if v.is_object() => v.clone(),
            _ => return Ok(None),
        };
        if !mcp
            .get("autoRegister")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(None);
        }
        let name = mcp_server_name(&mcp, app_id);
        let mut config =
            build_mcp_config(&name, &mcp, app_id, base_url, mcp_origin.as_deref(), session)?;
        stamp_app_identity(&mut config, db, app_id, self.api_version);
        manager
            .add_or_update(config, McpScopeType::Project)
            .await
            .with_context(|| format!("register MCP '{name}'"))?;
        // Remember what this app's tools are, so the next boot can put them in
        // the agent roster without starting the app to ask.
        let tools = manager
            .get_server_info(&name)
            .await
            .tools
            .unwrap_or_default();
        write_tool_cache(app_dir, &tools);
        // Import tool aliases declared in the manifest (`mcp.toolAliases`).
        // They land DISABLED — the user must approve each one in
        // Plugins → Alias before it takes effect.
        sync_app_tool_aliases(db, app_id, &name, &mcp);
        Ok(Some(name))
    }

    /// Ensure the app's server process is running and healthy; returns its port.
    /// Idempotent: an already-healthy server (tracked, manual, or orphaned on a
    /// fixed port) is reused rather than double-spawned.
    async fn ensure_server_running(
        &self,
        db: &Db,
        app_id: &str,
        app_dir: &Path,
        spec: &RuntimeSpec,
        requires: &Requires,
        base_url: &str,
    ) -> Result<u16> {
        let start = spec
            .start
            .as_deref()
            .ok_or_else(|| anyhow!("runtime.start is required for a server app"))?;
        let health_path = spec.health_path.as_str();
        let fixed_port = spec.port;

        // Reuse a tracked, still-alive child — or get rid of it.
        //
        // The third case is the one that used to leak. A child that is alive but
        // *not answering* fell through to the spawn below, and the insert at the
        // end overwrote the map entry — dropping the only handle to the first
        // process without killing it. It kept running, kept its port, and became
        // invisible: an orphan the launch counter dutifully counted past while
        // the user watched the same app start "6×".
        //
        // We can kill it without ceremony because this whole function is
        // serialized per app by `start_lock`: nobody else is mid-spawn, so an
        // alive-but-unhealthy child here is one whose own 30s health wait
        // already gave up, or one that wedged after it.
        let tracked_port = {
            let mut children = self.children.lock().await;
            match children.get_mut(app_id) {
                Some(proc) => {
                    if matches!(proc.child.try_wait(), Ok(None)) {
                        Some(proc.port)
                    } else {
                        // Exited on its own: drop the entry and spawn a fresh one.
                        children.remove(app_id);
                        None
                    }
                }
                None => None,
            }
        };
        // Probed outside the lock: an HTTP request holding the mutex that every
        // other app's bookkeeping needs is a stall waiting to happen.
        if let Some(port) = tracked_port {
            if self.is_healthy(&health_url(port, health_path)).await {
                return Ok(port);
            }
            tracing::warn!(
                "[space-mcp] '{app_id}' is running but not answering on :{port} — \
                 replacing it rather than starting a second copy"
            );
            if let Some(proc) = self.children.lock().await.remove(app_id) {
                kill_child_group(proc).await;
            }
            // Its port may outlive it by a moment (grandchildren, TIME_WAIT).
            kill_port_listeners(port).await;
            for _ in 0..20 {
                if port_is_free(port) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        // Fixed port already healthy, but nothing tracked is behind it — so it is
        // a process from a previous daemon (or a manual run). Adopting it used to
        // be the answer and it was the wrong one: the app then runs whatever code
        // it was started with, from wherever it was started, with none of the
        // sandbox settings applied, for as long as the machine stays up.
        //
        // Take the port back when the process is verifiably this app's, and start
        // it again properly. When it is not ours, adopt as before — that port
        // belongs to someone else's server and killing it would be hostile.
        if fixed_port > 0 && self.is_healthy(&health_url(fixed_port, health_path)).await {
            match reclaim_app_port(app_id, app_dir, fixed_port).await {
                Reclaim::Freed => {
                    tracing::info!(
                        "[space-mcp] '{app_id}': took :{fixed_port} back from an untracked \
                         process, launching a fresh one"
                    );
                }
                Reclaim::NotOurs => {
                    tracing::info!("[space-mcp] '{app_id}' already serving on :{fixed_port}");
                    self.adopted.lock().await.insert(app_id.to_string());
                    return Ok(fixed_port);
                }
                Reclaim::Failed => {
                    tracing::warn!(
                        "[space-mcp] '{app_id}': could not free :{fixed_port}; using the process \
                         that is there (its sandbox state is unknown)"
                    );
                    self.adopted.lock().await.insert(app_id.to_string());
                    return Ok(fixed_port);
                }
            }
        }

        // What the app said this machine must have. Checked here rather than at
        // install alone, because an install-time check answers for the machine
        // as it was that day: Homebrew uninstalls happen, and `nvm` changes
        // which node is on PATH. A refusal with a reason beats `exit 127` in a
        // log file.
        if !requires.is_empty() {
            let report = crate::apps::requirements::check(requires).await;
            if !report.satisfied {
                let hints: Vec<String> = report
                    .blocking()
                    .iter()
                    .map(|c| c.hint.clone())
                    .filter(|h| !h.is_empty())
                    .collect();
                return Err(anyhow!(
                    "'{app_id}' cannot start — {}. {}",
                    report.summary,
                    hints.join(" ")
                ));
            }
        }

        // Install the app's dependencies if it ships source rather than a
        // binary (`npm ci`, `pip install` into its own venv). A no-op after the
        // first launch, and for the native apps that are the majority.
        let prepared = crate::apps::prepare::prepare(app_id, app_dir, spec)
            .await
            .with_context(|| format!("prepare runtime for app '{app_id}'"))?;

        let port = if fixed_port > 0 {
            fixed_port
        } else {
            pick_free_port().ok_or_else(|| anyhow!("no free port for app '{app_id}'"))?
        };
        let log_path = app_runtime_log_path(app_dir);
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create log dir for app '{app_id}'"))?;
        }
        let mut log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("open runtime log for app '{app_id}'"))?;
        use std::io::Write as _;
        let _ = writeln!(
            log_file,
            "\n===== {} launching {app_id}: {start} (PORT={port}) =====",
            chrono::Utc::now().to_rfc3339()
        );
        let stdout = log_file
            .try_clone()
            .with_context(|| format!("clone stdout log for app '{app_id}'"))?;
        let stderr = log_file
            .try_clone()
            .with_context(|| format!("clone stderr log for app '{app_id}'"))?;

        // How this app is confined, if the user asked for it in Plugins → Space
        // Apps. `plan` returns the plain `sh -c <start>` when the app is not
        // sandboxed, so there is one spawn path either way. A failure here is
        // deliberately fatal: the user asked for a sandbox, and launching the app
        // unconfined instead would be the one outcome nobody wants.
        let sb_cfg = crate::sandbox::app_policy::current(app_id);
        let launch = crate::sandbox::app_launch::plan(
            app_id,
            app_dir,
            start,
            port,
            daemon_port(base_url),
            &sb_cfg,
        )
        .await
        .with_context(|| format!("prepare the sandbox for app '{app_id}'"))?;
        let _ = writeln!(log_file, "{}", launch.summary());
        for note in &prepared.notes {
            let _ = writeln!(log_file, "{note}");
        }
        if let Some(note) = &launch.note {
            tracing::warn!("[space-mcp] '{app_id}' sandbox: {note}");
        }

        // Spawn through the platform shell (wrapped by `sandbox-exec` / `bwrap`
        // when confined). On unix it gets its own process group so we can kill
        // the whole tree (npm -> next-server) on shutdown; on Windows we fall
        // back to killing the direct child.
        let mut cmd = Command::new(&launch.argv[0]);
        cmd.args(&launch.argv[1..]);
        cmd.current_dir(app_dir)
            .env("PORT", port.to_string())
            .env("SENCLAW_BASE_URL", base_url)
            .env("SENCLAW_SPACE_APP_ID", app_id)
            .env("SENCLAW_SPACE_LOG_FILE", &log_path)
            // The shared local-model root, for the engine apps. Passed
            // explicitly rather than left to the app's own `~/.senclaw`
            // fallback: the daemon's copy is overridable
            // (`SENCLAW_LOCAL_MODELS_DIR`), and an app that guessed would build
            // a *second* model library — tens of gigabytes, re-downloaded,
            // while the first one sits unused and invisible.
            .env(
                "SENCLAW_LOCAL_MODELS_DIR",
                crate::config::Config::from_env().paths.local_models_dir,
            )
            .stdin(Stdio::null())
            // Placeholder so the two SenClaw identity variables are always set
            // in the child, even on the error path below — an app that reads
            // an *inherited* SENCLAW_TOKEN_ACCESS_APP from the daemon's own
            // environment would authenticate as whatever the operator exported.
            .env_remove(crate::apps::token::ENV_APP_TOKEN)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true);
        for (k, v) in &launch.env {
            cmd.env(k, v);
        }
        // This app's identity to the daemon: the access token it presents on
        // /api/space/apps/<id>/… and the API contract it was launched under.
        // Minted on first launch, so an app installed before the feature gets
        // one without a migration. See src/apps/token.rs.
        for (k, v) in crate::apps::token::launch_env(db, app_id, self.api_version) {
            cmd.env(k, v);
        }
        // The interpreter environment the prepare step produced (a Python
        // venv's `PATH` / `VIRTUAL_ENV`). Applied after the sandbox's own env so
        // an app that needs its venv gets it either way; an empty value means
        // "unset", which is how a stray `PYTHONHOME` is cleared.
        for (k, v) in &prepared.env {
            if v.is_empty() {
                cmd.env_remove(k);
            } else {
                cmd.env(k, v);
            }
        }
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd
            .spawn()
            .with_context(|| format!("spawn '{start}' for app '{app_id}'"))?;
        let pgid = child.id().map(|i| i as i32).unwrap_or(0);
        // Ours again.
        self.adopted.lock().await.remove(app_id);
        *self
            .launches
            .lock()
            .await
            .entry(app_id.to_string())
            .or_insert(0) += 1;
        // Never overwrite a tracked child: the map holds the only handle to a
        // process, so replacing an entry silently orphans whatever it pointed
        // at. Nothing should reach here with one still tracked — every path
        // above removes or kills first — but "should" is how the orphans got
        // here in the first place, and one running app per app is the invariant
        // worth enforcing at the point it can actually be broken.
        if let Some(stale) = self.children.lock().await.remove(app_id) {
            tracing::warn!(
                "[space-mcp] '{app_id}': a previous process (pid {:?}) was still tracked at \
                 spawn time — killing it so only one copy runs",
                stale.child.id()
            );
            kill_child_group(stale).await;
        }
        self.children.lock().await.insert(
            app_id.to_string(),
            ChildProc {
                child,
                pgid,
                port,
                log_path: log_path.clone(),
                proxy: launch.proxy.clone(),
                started_at: std::time::SystemTime::now(),
                isolation: if launch.enforced {
                    launch.isolation.clone()
                } else {
                    "none".to_string()
                },
                mode: spec.mode,
                idle_timeout: Duration::from_secs(spec.idle_timeout_secs),
                // Starting counts as using it: an app launched by a tool call
                // must not be reaped before that call has finished.
                last_activity: std::time::Instant::now(),
            },
        );
        self.user_stopped.lock().await.remove(app_id);
        tracing::info!(
            "[space-mcp] launched '{app_id}' [{}]: {start} (PORT={port}, log={})",
            spec.mode.as_str(),
            log_path.display()
        );

        // Wait for health (server boot can take a few seconds).
        let url = health_url(port, health_path);
        for _ in 0..120 {
            if self.is_healthy(&url).await {
                return Ok(port);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(anyhow!("server app '{app_id}' not healthy at {url}"))
    }

    async fn is_healthy(&self, url: &str) -> bool {
        matches!(self.http.get(url).send().await, Ok(r) if r.status().is_success())
    }

    /// Stop one app's server process (on uninstall).
    pub async fn stop_app(&self, app_id: &str) {
        if let Some(proc) = self.children.lock().await.remove(app_id) {
            let log = proc.log_path.display().to_string();
            kill_child_group(proc).await;
            tracing::info!(
                "[space-mcp] stopped server process for '{app_id}' (uninstall, log={log})"
            );
        }
    }

    /// Everything the monitor needs about the process this daemon launched for
    /// `app_id`. `None` when nothing is tracked — which for a server app means
    /// it is not running, whatever the manifest says.
    pub async fn runtime_info(&self, app_id: &str) -> Option<RuntimeInfo> {
        let children = self.children.lock().await;
        let proc = children.get(app_id)?;
        Some(RuntimeInfo {
            pid: proc.child.id().unwrap_or(0),
            pgid: proc.pgid,
            port: proc.port,
            log_path: proc.log_path.clone(),
            started_at: proc.started_at,
            isolation: proc.isolation.clone(),
            proxy: proc.proxy.as_ref().map(|p| (p.port, p.stats())),
            mode: proc.mode.as_str(),
            idle_secs: std::time::Instant::now()
                .duration_since(proc.last_activity)
                .as_secs(),
            idle_timeout_secs: if proc.mode.is_background() {
                0
            } else {
                proc.idle_timeout.as_secs()
            },
        })
    }

    /// How many times this app has been launched since the daemon started.
    pub async fn launch_count(&self, app_id: &str) -> u32 {
        self.launches.lock().await.get(app_id).copied().unwrap_or(0)
    }

    /// Live sandbox facts for one app: the port of its allowlist proxy and what
    /// that proxy has refused so far. `None` when the app is not running, or is
    /// running without per-site egress.
    ///
    /// The refusal list is the reason this is exposed at all: "the app is broken"
    /// and "the app wanted `x.com` and did not have it" look identical from the
    /// outside otherwise.
    pub async fn proxy_status(&self, app_id: &str) -> Option<(u16, crate::sandbox::proxy::ProxyStats)> {
        let children = self.children.lock().await;
        let proxy = children.get(app_id)?.proxy.as_ref()?;
        Some((proxy.port, proxy.stats()))
    }

    /// Push an edited allowlist into a *running* app's proxy, so adding a site
    /// takes effect without restarting the app.
    pub async fn set_proxy_hosts(&self, app_id: &str, hosts: Vec<String>) -> bool {
        let children = self.children.lock().await;
        match children.get(app_id).and_then(|c| c.proxy.as_ref()) {
            Some(p) => {
                p.set_hosts(hosts);
                true
            }
            None => false,
        }
    }

    /// Kill every launched server process group. Call on graceful shutdown.
    ///
    /// Signals all of them first, then waits **once** — not app by app. The
    /// desktop app allows ~800 ms between SIGTERM and SIGKILL, and a machine
    /// with a few dozen apps would never finish a per-app grace period inside
    /// that window; every app that did not get its turn survived the daemon.
    pub async fn shutdown(&self) {
        let procs: Vec<(String, ChildProc)> = self.children.lock().await.drain().collect();
        if procs.is_empty() {
            return;
        }
        let n = procs.len();
        #[cfg(unix)]
        for (_, proc) in &procs {
            if proc.pgid > 0 {
                // Negative pid → the whole group (sh + npm + node, …).
                unsafe {
                    libc::kill(-proc.pgid, libc::SIGTERM);
                }
            }
        }
        // One grace period for everyone, well inside the window we are given.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut left = 0usize;
        for (app_id, mut proc) in procs {
            let log = proc.log_path.display().to_string();
            if matches!(proc.child.try_wait(), Ok(Some(_))) {
                tracing::info!("[space-mcp] stopped server process for '{app_id}' (log={log})");
                continue;
            }
            #[cfg(unix)]
            if proc.pgid > 0 {
                unsafe {
                    libc::kill(-proc.pgid, libc::SIGKILL);
                }
            }
            let _ = proc.child.start_kill();
            let _ = proc.child.try_wait();
            left += 1;
            tracing::info!("[space-mcp] killed server process for '{app_id}' (log={log})");
        }
        tracing::info!("[space-mcp] shutdown: {n} app(s) stopped ({left} needed SIGKILL)");
    }
}

/// Signal a launched app's whole process group (SIGTERM, then SIGKILL after a
/// grace period) and reap the direct child so no zombie/port-holder is left.
async fn kill_child_group(proc: ChildProc) {
    let pgid = proc.pgid;
    let mut child = proc.child;
    #[cfg(unix)]
    if pgid > 0 {
        // Negative pid → the whole group (sh + npm + next-server, …).
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
    }
    // Give it up to ~2s to exit cleanly on SIGTERM.
    for _ in 0..20 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    #[cfg(unix)]
    if pgid > 0 {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// True if nothing is currently bound to `127.0.0.1:port`.
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Best-effort SIGKILL of any process still LISTENing on `port`. Reclaims a
/// port held by an orphaned grandchild (npm/next-server) that survived an
/// earlier kill of only the `sh` group leader. Unix-only; no-op elsewhere.
/// What happened when we tried to take back a port an untracked process holds.
#[derive(Debug, PartialEq)]
pub(crate) enum Reclaim {
    /// It was this app's process, it is gone, the port is free.
    Freed,
    /// Something else is on that port. Not ours to kill.
    NotOurs,
    /// It is ours but would not die, or the port stayed busy.
    Failed,
}

/// A process's working directory, read with `lsof`. `None` when it cannot be
/// determined — which the caller must treat as "cannot verify", never as "yes".
pub(crate) async fn process_cwd(pid: i32) -> Option<PathBuf> {
    if cfg!(windows) || pid <= 0 {
        return None;
    }
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new("lsof")
            .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    // `-Fn` prints one field per line; the cwd path is the line starting with n.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n').map(PathBuf::from))
}

/// pids listening on `port`.
pub(crate) async fn pids_on_port(port: u16) -> Vec<i32> {
    if cfg!(windows) || port == 0 {
        return Vec::new();
    }
    let Ok(Ok(out)) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new("lsof")
            .args(["-ti", &format!("tcp:{port}"), "-sTCP:LISTEN"])
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<i32>().ok())
        .collect()
}

/// Take back an app's port from a process this daemon does not track — but
/// **only if that process is demonstrably the app's**.
///
/// This is the fix for a real state: the daemon used to die without stopping its
/// children (SIGTERM was not handled), so every restart left the previous
/// generation running. The next daemon found the ports healthy and adopted them,
/// which meant apps ran for weeks as orphans, from stale install directories,
/// with none of the sandbox settings applied.
///
/// The check is the process's working directory, because that is what the
/// launcher sets and what every runtime inherits (`sh -c "npm start"` → node in
/// the app dir). A process we cannot place inside the app's own directory is
/// left alone: a port collision with the user's own dev server must not turn the
/// daemon into something that kills it on every boot.
pub(crate) async fn reclaim_app_port(app_id: &str, app_dir: &Path, port: u16) -> Reclaim {
    let pids = pids_on_port(port).await;
    if pids.is_empty() {
        return Reclaim::Freed;
    }
    let want = app_dir.canonicalize().unwrap_or_else(|_| app_dir.to_path_buf());
    let mut killed = false;
    for pid in pids {
        let cwd = process_cwd(pid).await;
        let ours = cwd
            .as_ref()
            .map(|c| {
                let c = c.canonicalize().unwrap_or_else(|_| c.clone());
                c.starts_with(&want)
            })
            .unwrap_or(false);
        if !ours {
            tracing::warn!(
                "[space-mcp] port {port} for '{app_id}' is held by pid {pid} (cwd={:?}), which is \
                 not this app — leaving it alone and adopting instead",
                cwd
            );
            return Reclaim::NotOurs;
        }
        #[cfg(unix)]
        unsafe {
            // The whole group: the listener is often a grandchild (npm → node).
            let pgid = libc::getpgid(pid);
            if pgid > 0 {
                libc::kill(-pgid, libc::SIGTERM);
            } else {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        killed = true;
        tracing::info!("[space-mcp] '{app_id}': reclaiming :{port} from orphan pid {pid}");
    }
    if !killed {
        return Reclaim::NotOurs;
    }
    // Give it a moment, then insist.
    for i in 0..20 {
        if port_is_free(port) {
            return Reclaim::Freed;
        }
        if i == 5 {
            #[cfg(unix)]
            for pid in pids_on_port(port).await {
                unsafe {
                    let pgid = libc::getpgid(pid);
                    libc::kill(if pgid > 0 { -pgid } else { pid }, libc::SIGKILL);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Reclaim::Failed
}

async fn kill_port_listeners(port: u16) {
    if port == 0 {
        return;
    }
    #[cfg(unix)]
    {
        let out = tokio::process::Command::new("lsof")
            .args(["-ti", &format!("tcp:{port}"), "-sTCP:LISTEN"])
            .output()
            .await;
        if let Ok(out) = out {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Ok(pid) = line.trim().parse::<i32>() {
                    tracing::info!("[space-mcp] reclaiming port {port}: killing pid {pid}");
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = port;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test here that takes an ephemeral port shares this.
    ///
    /// `pick_free_port` binds, reads the number and releases — so two tests
    /// running in parallel can be handed the *same* port, and then one of them
    /// sees a socket it did not open. That is a genuine flake (caught in one run
    /// out of ten), not a code bug, and serialising is what fixes it.
    static PORT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn port_is_free_reflects_binding() {
        let _guard = PORT_LOCK.blocking_lock();
        // A port we hold a listener on is not free; once released it is.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_free(port), "held port must report busy");
        drop(listener);
        assert!(port_is_free(port), "released port must report free");
    }

    #[test]
    fn a_session_apps_mcp_is_addressed_through_the_daemon_not_its_own_port() {
        // The whole on-demand mechanism rests on this URL: the app's own port
        // has nothing behind it while it is stopped, and the proxy is what
        // starts it. Pointing at `127.0.0.1:<app port>` instead would work
        // exactly until the first idle timeout.
        let origin = session_mcp_origin("http://127.0.0.1:18788/", "crm");
        assert_eq!(origin, "http://127.0.0.1:18788/api/space/apps/crm/proxy");
        let mcp = serde_json::json!({"path": "/api/mcp/sse", "url": "http://127.0.0.1:4390/api/mcp/sse"});
        let cfg = build_mcp_config("crm-mcp", &mcp, "crm", "http://127.0.0.1:18788", Some(&origin), true)
            .unwrap();
        assert_eq!(
            cfg.url.as_deref(),
            Some("http://127.0.0.1:18788/api/space/apps/crm/proxy/api/mcp/sse"),
            "a declared absolute url must not win for a session app"
        );
        // A background app keeps talking straight to its own port.
        let cfg = build_mcp_config(
            "crm-mcp",
            &mcp,
            "crm",
            "http://127.0.0.1:18788",
            Some("http://127.0.0.1:4390"),
            false,
        )
        .unwrap();
        assert_eq!(cfg.url.as_deref(), Some("http://127.0.0.1:4390/api/mcp/sse"));
    }

    #[test]
    fn the_mcp_server_name_is_the_manifests_not_a_guess() {
        // luna-calendar registers `luna-mcp`; deriving `<id>-mcp` would look up
        // a server that does not exist and silently do nothing.
        let named = serde_json::json!({"name": "luna-mcp"});
        assert_eq!(mcp_server_name(&named, "luna-calendar"), "luna-mcp");
        assert_eq!(mcp_server_name(&serde_json::json!({}), "crm"), "crm-mcp");
    }

    #[test]
    fn a_failed_connect_never_erases_the_cached_tool_list() {
        // The cache is what puts a stopped app's tools in the agent roster. If
        // an empty list could overwrite it, one failed connect would make the
        // app uncallable — and therefore unstartable — until a manual restart.
        let dir = tempfile::tempdir().unwrap();
        let tools = vec![McpToolDef {
            name: "crm_search".into(),
            description: Some("find".into()),
            input_schema: None,
        }];
        write_tool_cache(dir.path(), &tools);
        assert_eq!(read_tool_cache(dir.path()).len(), 1);
        write_tool_cache(dir.path(), &[]);
        assert_eq!(read_tool_cache(dir.path()).len(), 1, "empty must not clobber");
        assert!(read_tool_cache(tempfile::tempdir().unwrap().path()).is_empty());
    }

    /// A real listener holding a real port, with a working directory we choose —
    /// the two facts `reclaim_app_port` decides on.
    async fn spawn_listener(cwd: &Path, port: u16) -> tokio::process::Child {
        let script = format!(
            "import socket,time
s=socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1',{port}))
s.listen()
time.sleep(60)"
        );
        let mut cmd = tokio::process::Command::new("python3");
        cmd.arg("-c")
            .arg(script)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // Its own process group, exactly like a launched app — without this the
        // group kill under test takes the test runner down with it, which is
        // also the reason the daemon must never signal a group it did not create.
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd.spawn().expect("spawn a listener");
        for _ in 0..40 {
            if !port_is_free(port) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        child
    }

    fn a_free_port() -> u16 {
        pick_free_port().expect("a free port")
    }

    #[tokio::test]
    async fn a_process_is_only_killed_when_it_is_demonstrably_the_app() {
        let _guard = PORT_LOCK.lock().await;
        // The safety property: a port collision with something that is not this
        // app — the user's own dev server — must not turn the daemon into
        // something that kills it on every boot.
        let app_dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let port = a_free_port();
        let mut foreign = spawn_listener(elsewhere.path(), port).await;
        if port_is_free(port) {
            return; // python3 unavailable on this machine; nothing to assert
        }

        let outcome = reclaim_app_port("demo", app_dir.path(), port).await;
        assert_eq!(outcome, Reclaim::NotOurs);
        assert!(
            matches!(foreign.try_wait(), Ok(None)),
            "a process outside the app directory must survive"
        );
        let _ = foreign.kill().await;
    }

    #[tokio::test]
    async fn an_orphan_of_this_app_is_killed_and_its_port_freed() {
        let _guard = PORT_LOCK.lock().await;
        // The state this exists to clean up: a previous daemon's child still
        // holding the app's port days later.
        let app_dir = tempfile::tempdir().unwrap();
        let port = a_free_port();
        let mut orphan = spawn_listener(app_dir.path(), port).await;
        if port_is_free(port) {
            return; // no python3 here
        }

        let outcome = reclaim_app_port("demo", app_dir.path(), port).await;
        assert_eq!(outcome, Reclaim::Freed);
        assert!(port_is_free(port), "the port must be usable again");
        assert!(
            !matches!(orphan.try_wait(), Ok(None)),
            "the orphan must be gone, not merely signalled"
        );
    }

    #[tokio::test]
    async fn a_free_port_needs_no_reclaiming() {
        let _guard = PORT_LOCK.lock().await;
        let app_dir = tempfile::tempdir().unwrap();
        assert_eq!(
            reclaim_app_port("demo", app_dir.path(), a_free_port()).await,
            Reclaim::Freed
        );
    }

    #[tokio::test]
    async fn the_working_directory_of_a_live_process_is_readable() {
        // `process_cwd` returning None must mean "cannot verify", so this pins
        // the happy path it is judged against.
        let dir = tempfile::tempdir().unwrap();
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 5")
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap() as i32;
        let cwd = process_cwd(pid).await;
        let _ = child.kill().await;
        if let Some(cwd) = cwd {
            let want = dir.path().canonicalize().unwrap();
            assert_eq!(cwd.canonicalize().unwrap(), want);
        }
    }

    #[test]
    fn the_daemon_port_is_taken_from_the_url_the_app_is_given() {
        // A sandboxed app gets exactly this one loopback port for the AI bridge,
        // so reading the wrong number means either a broken bridge or an open
        // door to some other local service.
        assert_eq!(daemon_port("http://127.0.0.1:18788"), 18788);
        assert_eq!(daemon_port("http://127.0.0.1:18788/"), 18788);
        assert_eq!(daemon_port("http://localhost:9000/api"), 9000);
        // No port in the URL: fall back to the documented default rather than
        // guessing something reachable.
        assert_eq!(daemon_port("http://127.0.0.1"), 18788);
        assert_eq!(daemon_port(""), 18788);
    }
}

/// Every enabled installed app, id + manifest. The three loops that walk the
/// app list (boot, supervise, reap) all want exactly this.
fn enabled_apps(db: &Db) -> Vec<(String, Value)> {
    match db.with_conn(|conn| {
        let mut stmt = conn.prepare("SELECT id, manifest FROM space_apps WHERE enabled = 1")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .filter_map(|(id, m)| serde_json::from_str::<Value>(&m).ok().map(|v| (id, v)))
            .collect::<Vec<_>>();
        Ok(rows)
    }) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("[space-mcp] could not list space apps: {e}");
            Vec::new()
        }
    }
}

/// The MCP server name for an app: `mcp.name`, else `<id>-mcp`. Derived in one
/// place because the name is what every later lookup keys on, and an app whose
/// manifest names it something else (luna-calendar → `luna-mcp`) must not be
/// found under a guessed name.
pub(crate) fn mcp_server_name(mcp: &Value, app_id: &str) -> String {
    mcp.get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{app_id}-mcp"))
}

/// Where a session app's MCP is addressed: the daemon's own app proxy, which
/// starts the app before forwarding. `mcp.path` is appended to this.
pub(crate) fn session_mcp_origin(base_url: &str, app_id: &str) -> String {
    format!(
        "{}/api/space/apps/{app_id}/proxy",
        base_url.trim_end_matches('/')
    )
}

/// Where an app's last-known tool list is kept.
fn tool_cache_path(app_dir: &Path) -> PathBuf {
    app_dir.join(".senclaw").join("mcp-tools.json")
}

/// The tools this app reported the last time it was connected to. This is what
/// lets a stopped session app still have tools in the agent roster — without
/// it, nothing would ever call one and it would never start.
pub(crate) fn read_tool_cache(app_dir: &Path) -> Vec<McpToolDef> {
    std::fs::read_to_string(tool_cache_path(app_dir))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<McpToolDef>>(&s).ok())
        .unwrap_or_default()
}

fn write_tool_cache(app_dir: &Path, tools: &[McpToolDef]) {
    if tools.is_empty() {
        // Never overwrite a good cache with an empty one: a connect that failed
        // would otherwise erase the roster the next boot depends on.
        return;
    }
    let path = tool_cache_path(app_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(tools) {
        let _ = std::fs::write(path, json);
    }
}

/// Cache what a running app answered on `/v1/models`.
///
/// The app writes this itself through `app_space_sdk::llm::publish_models`, but
/// an app that does not call it would never be registerable while stopped. So
/// the daemon writes it too, from the answer it just received.
///
/// Empty never overwrites, for the same reason as [`write_tool_cache`]: a failed
/// fetch would otherwise erase the list the next boot registers from, and the
/// app's models would silently leave the picker.
fn write_models_cache(app_dir: &Path, models: &[crate::apps::llm_provider::ModelCard]) {
    if models.is_empty() {
        return;
    }
    if let Err(e) = app_space_sdk::llm::publish_models(app_dir, models) {
        tracing::debug!("[app-llm] model cache not written to {app_dir:?}: {e}");
    }
}

/// Where the app's files live: an explicit `install.localPath`, else
/// `<apps_dir>/<app_id>`.
fn app_install_dir(manifest: &Value, apps_dir: &Path, app_id: &str) -> PathBuf {
    manifest
        .get("install")
        .and_then(|i| i.get("localPath"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| apps_dir.join(app_id))
}

pub fn app_runtime_log_path(app_dir: &Path) -> PathBuf {
    app_dir.join(".senclaw").join("runtime.log")
}

fn health_url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

/// SenClaw's own UI port, read off the base URL the app is given. A sandboxed
/// app that may use the AI bridge needs exactly this port on loopback and
/// nothing else, so it is derived from the same string the app dials rather than
/// re-read from the environment (where it could disagree).
fn daemon_port(base_url: &str) -> u16 {
    base_url
        .rsplit_once(':')
        .and_then(|(_, p)| {
            p.trim_end_matches('/')
                .split('/')
                .next()
                .and_then(|n| n.parse::<u16>().ok())
        })
        .unwrap_or(18788)
}

fn pick_free_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

/// Sync the app's declared tool aliases (`mcp.toolAliases`) into the
/// `mcp_tool_aliases` table. Idempotent: re-imports refresh target/description
/// but never touch `enabled` (the user's opt-in from Plugins → Alias), and
/// aliases the manifest no longer declares are pruned. Runs on every path that
/// registers the app's MCP — install, update, boot, supervisor respawn.
fn sync_app_tool_aliases(db: &Db, app_id: &str, server_name: &str, mcp: &Value) {
    let declared = crate::tools::tool_alias::parse_declared_aliases(server_name, mcp);
    let keep: Vec<String> = declared.iter().map(|d| d.alias.clone()).collect();
    for d in &declared {
        if let Err(e) =
            db.import_app_tool_alias(app_id, &d.alias, &d.target, d.description.as_deref())
        {
            tracing::warn!("[space-mcp] {app_id}: import tool alias '{}' failed: {e:#}", d.alias);
        }
    }
    match db.prune_app_tool_aliases(app_id, &keep) {
        Ok(n) if n > 0 => {
            tracing::info!("[space-mcp] {app_id}: pruned {n} stale tool alias(es)");
        }
        Err(e) => tracing::warn!("[space-mcp] {app_id}: prune tool aliases failed: {e:#}"),
        _ => {}
    }
    if !keep.is_empty() {
        tracing::info!(
            "[space-mcp] {app_id}: imported {} declared tool alias(es) (disabled until approved in Plugins → Alias)",
            keep.len()
        );
    }
    crate::tools::tool_alias::reload_from_db(db);
}

fn update_app_manifest(db: &Db, app_id: &str, manifest: &Value) {
    let raw = serde_json::to_string(manifest).unwrap_or_default();
    let now = chrono::Utc::now().timestamp_millis();
    let _ = db.with_conn(|conn| {
        conn.execute(
            "UPDATE space_apps SET manifest=?1, last_seen_at=?2 WHERE id=?3",
            params![raw, now, app_id],
        )?;
        Ok(())
    });
}

/// Stamp the app's identity onto its registered MCP server config.
///
/// Two transports, two carriers, one reason: whatever dials this server must be
/// able to prove which app it is addressing.
///
/// - **stdio** — the child process is the app, so the token goes in its
///   environment exactly as it does for a server app.
/// - **http / sse** — the token travels as a header on every call. This is what
///   keeps a *background* app reachable once it enforces the guard: the agent's
///   MCP client dials the app's own port directly, never passing through the
///   daemon's proxy, and would otherwise arrive with nothing to show.
fn stamp_app_identity(
    config: &mut ExternalMcpServerConfig,
    db: &Db,
    app_id: &str,
    api_version: u32,
) {
    for (k, v) in crate::apps::token::launch_env(db, app_id, api_version) {
        match config.transport {
            McpTransportType::Stdio => {
                config.env.insert(k, v);
            }
            _ => {
                let header = if k == crate::apps::token::ENV_APP_TOKEN {
                    crate::apps::token::HEADER_APP_TOKEN
                } else {
                    crate::apps::token::HEADER_API_VERSION
                };
                config.headers.insert(header.to_string(), v);
            }
        }
    }
}

/// Map a manifest `mcp` block onto an `ExternalMcpServerConfig`. For a server
/// app the URL is composed from the running `origin` + `mcp.path` unless an
/// absolute `mcp.url` is given.
fn build_mcp_config(
    name: &str,
    mcp: &Value,
    app_id: &str,
    base_url: &str,
    origin: Option<&str>,
    // A session app must be addressed through the proxy even if it declared an
    // absolute `mcp.url` — that URL names its own port, which is empty whenever
    // the app is stopped, and dialling it would never start anything.
    force_origin: bool,
) -> Result<ExternalMcpServerConfig> {
    let transport_str = mcp
        .get("transport")
        .and_then(Value::as_str)
        .unwrap_or("http");
    let transport = match transport_str {
        "stdio" => McpTransportType::Stdio,
        "sse" => McpTransportType::Sse,
        "http" => McpTransportType::Http,
        other => return Err(anyhow!("unknown mcp transport '{other}'")),
    };

    let str_field = |k: &str| mcp.get(k).and_then(Value::as_str).map(str::to_string);
    let str_array = |k: &str| {
        mcp.get(k).and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
    };
    let str_map = |k: &str| -> HashMap<String, String> {
        mcp.get(k)
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    };

    // Resolve the URL: absolute mcp.url wins; else origin + mcp.path.
    let declared_url = str_field("url").filter(|_| !force_origin);
    let url = match (declared_url, origin) {
        (Some(u), _) if u.starts_with("http") => Some(u),
        (_, Some(origin)) => {
            let path = mcp.get("path").and_then(Value::as_str).unwrap_or("/mcp");
            Some(format!("{}{}", origin.trim_end_matches('/'), path))
        }
        (other, None) => other,
    };

    let mut env = str_map("env");
    env.insert("SENCLAW_SPACE_APP_ID".into(), app_id.to_string());
    env.insert("SENCLAW_BASE_URL".into(), base_url.to_string());

    let (command, args) = if matches!(transport, McpTransportType::Stdio) {
        (str_field("command"), str_array("args").unwrap_or_default())
    } else {
        (None, vec![])
    };

    let config = ExternalMcpServerConfig {
        name: name.to_string(),
        transport,
        description: str_field("description"),
        enabled: true,
        use_tools: str_array("use_tools"),
        command,
        args,
        env,
        url,
        headers: str_map("headers"),
    };
    config.validate().map_err(|e| anyhow!(e))?;
    Ok(config)
}

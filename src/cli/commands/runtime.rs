//! `senclaw runtime` — install, select and inspect engine runtimes.
//!
//! Talks to the running daemon's REST (`/api/runtimes/*`) when one answers,
//! so a live daemon's in-process state (running processes, single-flight
//! installs) stays authoritative. When no daemon is reachable, each
//! subcommand falls back to operating on the runtime store directly — the
//! same files the daemon would read on its next start. Each subcommand's
//! `--help` says which path it took.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use sen_runtime_sdk::manifest::Slot;

use crate::config::Config;
use crate::runtime::manager::{RuntimeManager, RuntimeManagerConfig};

#[derive(Subcommand, Debug)]
pub enum RuntimeCmd {
    /// List installed runtimes and slot selections
    List,
    /// Install a runtime from the index (`<id>` or `<id>@<version>`)
    Install { target: String },
    /// Install a runtime from a local directory or `.tar.gz`/`.zip` archive
    InstallLocal { path: PathBuf },
    /// Remove an installed runtime version
    Uninstall {
        id: String,
        version: String,
        /// Stop it first if it is running
        #[arg(long)]
        force: bool,
    },
    /// Select which runtime fills a slot (`<id>` or `<id>@<version>`; omit to clear)
    Select { slot: String, target: Option<String> },
    /// Fetch the index and install updates for selected runtimes (if auto-update is on)
    Update,
    /// Show a runtime's recent log lines
    Logs {
        id: String,
        #[arg(long, default_value_t = 200)]
        lines: usize,
    },
}

fn api_base(cfg: &Config) -> String {
    format!("http://127.0.0.1:{}", cfg.ui_server.port)
}

/// Same file `auth.rs` writes: the token lives beside `config.json`. Only
/// needed under `SENCLAW_AUTH_MODE=always` — a loopback caller is otherwise
/// exempt.
fn auth_header(cfg: &Config) -> Option<String> {
    if let Some(t) = &cfg.ui_server.api_token {
        return Some(t.clone());
    }
    let dir = cfg.paths.global_config_path.parent()?;
    std::fs::read_to_string(dir.join("api_token")).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// `None` when the daemon is simply not running — the signal every
/// subcommand uses to fall back to the local store instead of failing.
async fn try_daemon(
    cfg: &Config,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<Option<serde_json::Value>> {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build()?;
    let mut req = client.request(method, format!("{}{path}", api_base(cfg)));
    if let Some(t) = auth_header(cfg) {
        req = req.header("X-SenClaw-Token", t);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|j| j.get("error").and_then(|v| v.as_str()).map(str::to_string))
            .unwrap_or_else(|| text.trim().to_string());
        bail!("{}", if msg.is_empty() { status.to_string() } else { msg });
    }
    Ok(Some(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)))
}

/// A `RuntimeManager` pointed at the real, on-disk store — used when no
/// daemon answers. Never launches a process supervisor loop; a one-shot CLI
/// invocation just needs the store/settings/index logic.
fn local_manager(cfg: &Config) -> std::sync::Arc<RuntimeManager> {
    RuntimeManager::new(RuntimeManagerConfig {
        runtimes_dir: cfg.paths.runtimes_dir.clone(),
        runtime_data_dir: cfg.paths.runtime_data_dir.clone(),
        runtime_logs_dir: cfg.paths.runtime_logs_dir.clone(),
        bundled_dir: cfg.paths.bundled_runtimes_dir.clone(),
        local_models_dir: cfg.paths.local_models_dir.clone(),
        config_path: cfg.paths.global_config_path.clone(),
        home: cfg.paths.global_config_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(".")),
        index_url: cfg.paths.runtime_index_url.clone(),
    })
}

fn split_target(target: &str) -> (String, Option<String>) {
    match target.split_once('@') {
        Some((id, v)) => (id.to_string(), Some(v.to_string())),
        None => (target.to_string(), None),
    }
}

pub async fn run(cmd: RuntimeCmd) -> Result<()> {
    let cfg = Config::from_env();
    match cmd {
        RuntimeCmd::List => {
            if let Some(v) = try_daemon(&cfg, reqwest::Method::GET, "/api/runtimes", None).await? {
                println!("(from the running daemon)");
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!("(daemon not running — reading the store directly)");
            let mgr = local_manager(&cfg);
            for pkg in mgr.installed() {
                println!(
                    "{} {} [{}] slots={:?} source={:?}",
                    pkg.manifest.id, pkg.manifest.version, pkg.manifest.name, pkg.manifest.slots, pkg.source
                );
            }
        }
        RuntimeCmd::Install { target } => {
            let (id, version) = split_target(&target);
            let body = serde_json::json!({"id": id, "version": version});
            if let Some(v) = try_daemon(&cfg, reqwest::Method::POST, "/api/runtimes/install", Some(body)).await? {
                println!("install started: {v}");
                return Ok(());
            }
            println!("(daemon not running — installing directly)");
            let mgr = local_manager(&cfg);
            let index = mgr.refresh_index().await.index;
            let entry = index.entry(&id).with_context(|| format!("`{id}` is not in the runtime index"))?;
            let version = version
                .or_else(|| entry.channel_version(mgr.settings().channel).map(str::to_string))
                .with_context(|| format!("`{id}` has no version for the configured channel"))?;
            let pkg = if entry.upstream.is_some() {
                // Direct CLI install path (no daemon running to relay a real
                // cancel): a fresh, never-triggered token.
                let cancel = tokio_util::sync::CancellationToken::new();
                crate::runtime::llamacpp::install(&cfg.paths.runtimes_dir, entry, &version, &|_, _| {}, &cancel).await?
            } else {
                let platform = sen_runtime_sdk::platform::current();
                let release = entry.release(&version).with_context(|| format!("`{id}` has no release `{version}`"))?;
                let package = entry
                    .package_for(release, platform)
                    .with_context(|| format!("`{id}` `{version}` has no package for `{platform}`"))?;
                let tmp = std::env::temp_dir().join(format!("senclaw-runtime-install-{}", uuid::Uuid::new_v4()));
                download_to_file(&package.url, &tmp).await?;
                let result = crate::runtime::store::install_from_archive(
                    &cfg.paths.runtimes_dir,
                    &tmp,
                    crate::runtime::store::PackageSource::Index,
                );
                let _ = std::fs::remove_file(&tmp);
                result?
            };
            println!("installed {} {}", pkg.manifest.id, pkg.manifest.version);
        }
        RuntimeCmd::InstallLocal { path } => {
            let abs = crate::util::paths::expand_tilde(&path.to_string_lossy());
            let body = serde_json::json!({"path": abs.to_string_lossy()});
            if let Some(v) = try_daemon(&cfg, reqwest::Method::POST, "/api/runtimes/install-local", Some(body)).await? {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!("(daemon not running — installing directly)");
            let pkg = crate::runtime::store::install_local(&cfg.paths.runtimes_dir, &abs)?;
            println!("installed {} {}", pkg.manifest.id, pkg.manifest.version);
        }
        RuntimeCmd::Uninstall { id, version, force } => {
            let path = format!("/api/runtimes/{id}/versions/{version}{}", if force { "?force=1" } else { "" });
            if try_daemon(&cfg, reqwest::Method::DELETE, &path, None).await?.is_some() {
                println!("uninstalled {id} {version}");
                return Ok(());
            }
            println!("(daemon not running — removing from the store directly)");
            crate::runtime::store::uninstall(&cfg.paths.runtimes_dir, &id, &version)?;
            println!("uninstalled {id} {version}");
        }
        RuntimeCmd::Select { slot, target } => {
            let slot_parsed = Slot::parse(&slot).with_context(|| format!("`{slot}` is not a slot"))?;
            let (id, version): (Option<String>, Option<String>) = match &target {
                Some(t) => {
                    let (id, version) = split_target(t);
                    (Some(id), version)
                }
                None => (None, None),
            };
            let body = serde_json::json!({"slot": slot, "id": id, "version": version});
            if let Some(v) = try_daemon(&cfg, reqwest::Method::PUT, "/api/runtimes/selections", Some(body)).await? {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!("(daemon not running — updating settings.json directly)");
            let mgr = local_manager(&cfg);
            mgr.select(slot_parsed, id, version)?;
            println!("selected {slot} -> {}", target.as_deref().unwrap_or("(cleared)"));
        }
        RuntimeCmd::Update => {
            if let Some(v) = try_daemon(&cfg, reqwest::Method::POST, "/api/runtimes/check-updates", None).await? {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!("(daemon not running — checking and installing updates directly)");
            let mgr = local_manager(&cfg);
            let cached = mgr.refresh_index().await;
            let (updates, started) = crate::runtime::updates::check_updates(&mgr, &cached).await;
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({"updates": updates, "started": started}))?);
        }
        RuntimeCmd::Logs { id, lines } => {
            let path = format!("/api/runtimes/{id}/logs?lines={lines}");
            if let Some(v) = try_daemon(&cfg, reqwest::Method::GET, &path, None).await? {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            println!("(daemon not running — reading the log file directly)");
            let mgr = local_manager(&cfg);
            let (path, log_lines) = mgr.logs(&id, lines, None)?;
            println!("{}", path.display());
            for line in log_lines {
                println!("{line}");
            }
        }
    }
    Ok(())
}

async fn download_to_file(url: &str, dest: &Path) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let client = reqwest::Client::builder().user_agent(format!("senclaw/{}", env!("CARGO_PKG_VERSION"))).build()?;
    let resp = client.get(url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    let mut file = tokio::fs::File::create(dest).await?;
    file.write_all(&bytes).await?;
    Ok(())
}

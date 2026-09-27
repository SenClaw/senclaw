//! PluginManager — spawn, track, and kill plugin subprocesses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tracing::{info, warn};

use super::db::{upsert_runtime, PluginRuntime};
use super::manifest::parse_plugin_md;
use crate::db::Db;

struct RunningProcess {
    child: Child,
}

#[derive(Default)]
struct Inner {
    procs: HashMap<String, RunningProcess>,
}

/// Manages lifecycle of plugin subprocesses.
pub struct PluginManager {
    db: Arc<Db>,
    inner: Mutex<Inner>,
}

impl PluginManager {
    pub fn new(db: Arc<Db>) -> Self {
        Self {
            db,
            inner: Mutex::default(),
        }
    }

    /// Spawn the plugin subprocess for `slug` using env from `config_json`.
    pub async fn start(&self, slug: &str, plugin_dir: &PathBuf, config_json: &str) -> Result<()> {
        let manifest_path = plugin_dir.join("PLUGIN.md");
        let manifest = parse_plugin_md(&manifest_path)
            .with_context(|| format!("missing PLUGIN.md for plugin `{slug}`"))?;

        let entry = manifest
            .entry_point
            .as_deref()
            .with_context(|| format!("plugin `{slug}` has no entry_point"))?;

        let binary = plugin_dir.join(entry);
        if !binary.exists() {
            anyhow::bail!("plugin binary not found: {}", binary.display());
        }

        let env_vals: HashMap<String, String> =
            serde_json::from_str(config_json).unwrap_or_default();

        let mut cmd = Command::new(&binary);
        cmd.current_dir(plugin_dir);
        for key in &manifest.env_vars {
            if let Some(val) = env_vals.get(key) {
                cmd.env(key, val);
            }
        }
        // Pipe stdio so we can capture logs
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn plugin `{slug}`"))?;

        let pid = child.id().map(|p| p as i64);
        info!(slug, pid, "plugin process started");

        let rt = PluginRuntime {
            slug: slug.to_string(),
            status: "running".to_string(),
            pid,
            port: None,
            started_at: Some(chrono::Utc::now().timestamp_millis()),
            error_msg: None,
            last_ping: None,
        };
        let _ = upsert_runtime(&self.db, &rt);

        self.inner
            .lock()
            .unwrap()
            .procs
            .insert(slug.to_string(), RunningProcess { child });
        Ok(())
    }

    /// Kill the subprocess for `slug` gracefully (SIGTERM → wait 3s → SIGKILL).
    pub async fn stop(&self, slug: &str) -> Result<()> {
        let child = {
            let mut guard = self.inner.lock().unwrap();
            guard.procs.remove(slug)
        };
        if let Some(mut proc) = child {
            if let Err(e) = proc.child.kill().await {
                warn!(slug, error = %e, "failed to kill plugin process");
            }
            let _ = proc.child.wait().await;
        }

        let rt = PluginRuntime {
            slug: slug.to_string(),
            status: "stopped".to_string(),
            pid: None,
            port: None,
            started_at: None,
            error_msg: None,
            last_ping: Some(chrono::Utc::now().timestamp_millis()),
        };
        let _ = upsert_runtime(&self.db, &rt);
        info!(slug, "plugin process stopped");
        Ok(())
    }

    pub fn is_running(&self, slug: &str) -> bool {
        self.inner.lock().unwrap().procs.contains_key(slug)
    }

    /// Kill all running plugins (called on daemon shutdown).
    pub async fn stop_all(&self) {
        let slugs: Vec<String> = self.inner.lock().unwrap().procs.keys().cloned().collect();
        for slug in slugs {
            let _ = self.stop(&slug).await;
        }
    }
}

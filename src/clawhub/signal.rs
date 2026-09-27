//! Skills hot-reload signal. Mirrors `src-old/clawhub/signal.ts`.
//!
//! CLI writes a signal file after install/update/uninstall; the daemon
//! watches it and triggers reloadAllSkills() in AgentPool.
//!
//! Signal file: `~/.senclaw/managed/skills/.clawhub/reload-signal`

use std::fs;
use std::path::PathBuf;

use crate::config::Config;

pub fn get_skills_reload_signal_path(config: &Config) -> PathBuf {
    config
        .paths
        .managed_skills_dir
        .join(".clawhub")
        .join("reload-signal")
}

/// Write the signal file to notify the daemon to reload skill registries.
pub fn emit_skills_refresh(config: &Config) -> Result<(), anyhow::Error> {
    let signal_path = get_skills_reload_signal_path(config);
    if let Some(parent) = signal_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = serde_json::json!({ "ts": chrono::Utc::now().timestamp_millis() });
    fs::write(&signal_path, payload.to_string())?;
    Ok(())
}

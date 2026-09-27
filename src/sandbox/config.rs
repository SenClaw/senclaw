//! Sandbox paths and knobs. Ported from `apps/sandbox/src/config.rs`; the data
//! root moves from the Space-App tree to `~/.senclaw/sandbox/` because the
//! engine now ships inside the daemon.
//!
//! `SANDBOX_DATA_DIR` is still honoured (the runner test-suite and existing
//! Space-App installs use it), with `SENCLAW_SANDBOX_DATA_DIR` as the
//! daemon-flavoured spelling checked first.

use std::path::PathBuf;

fn env_or(key: &str, default: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => default.to_string(),
    }
}

/// Sandbox data root — deliberately its own directory under `~/.senclaw`, NOT
/// the Space-App data dir: the engine must work with no Space App installed.
pub fn data_dir() -> PathBuf {
    for key in ["SENCLAW_SANDBOX_DATA_DIR", "SANDBOX_DATA_DIR"] {
        if let Ok(d) = std::env::var(key) {
            if !d.trim().is_empty() {
                return PathBuf::from(d);
            }
        }
    }
    match std::env::var("HOME").ok().filter(|h| !h.is_empty()) {
        Some(home) => PathBuf::from(home).join(".senclaw").join("sandbox"),
        None => PathBuf::from("."),
    }
}

pub fn db_path() -> String {
    data_dir().join("sandbox.sqlite").to_string_lossy().to_string()
}

/// Root holding one directory per sandbox. This is the ONLY host path a
/// sandbox may write to, and each sandbox is confined to its own subdirectory.
pub fn workspaces_dir() -> PathBuf {
    data_dir().join("workspaces")
}

/// Docker binary. Overridable because Docker Desktop, Colima, OrbStack and
/// Podman-in-docker-mode all ship the CLI at different paths.
pub fn docker_bin() -> String {
    env_or("SANDBOX_DOCKER_BIN", "docker")
}

/// Default image for docker-backed sandboxes.
pub fn default_image() -> String {
    env_or("SANDBOX_DEFAULT_IMAGE", "python:3.12-slim")
}

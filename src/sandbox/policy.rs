//! Enforcement policy: which daemon execution surfaces are pushed through the
//! OS sandbox, each behind its own on/off switch (Plugins → Sandbox in the
//! Web UI, or `/api/sandbox/exec-policy`).
//!
//!   - `exec_shell`        — the agent's `Bash` tool runs inside the sandbox:
//!                            writes are confined to the chat's working
//!                            directory, reads follow `exec_fs_mode`, network
//!                            follows `exec_network`.
//!   - `run_python` / `run_node` — the `/api/code/run` REPL may run real
//!                            Python / Node.js. These runtimes ONLY exist via
//!                            the sandbox: switching them off refuses the
//!                            language, never falls back to raw execution.
//!   - `scheduler_script`  — scheduler `script` / `script-agent` tasks run
//!                            their command inside a throwaway sandbox instead
//!                            of raw `bash -c` on the host.
//!
//! Defaults are chosen so that nothing existing breaks the day the feature
//! ships: agent Bash and scheduler scripts keep their historical behaviour
//! until the user opts in, while python/node — which did not exist before —
//! start enabled because they were never available un-sandboxed.

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::sandbox::db::{Db, NewSandbox, Run};
use crate::sandbox::fsmode::FsMode;
use crate::sandbox::{runner, shared_db};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExecPolicy {
    /// Agent `Bash` tool runs inside the OS sandbox (write-jail to the chat
    /// working directory).
    pub exec_shell: bool,
    /// Network for enforced Bash runs. On by default: agents legitimately
    /// build and install things; the write-jail is the protection being bought.
    pub exec_network: bool,
    /// Read isolation for enforced Bash runs. `open` by default — the agent
    /// could already read everything through its own Read tool, so starting
    /// stricter here would only break shell workflows without closing a door.
    pub exec_fs_mode: FsMode,
    /// Services on this machine the agent's shell may call. Empty by default:
    /// loopback is where SenClaw's own unauthenticated API lives, along with
    /// every Space App, so a sandbox that may dial it is not sandboxed. Name a
    /// port here when the agent legitimately needs one — the dev server it is
    /// working on, typically.
    #[serde(default)]
    pub exec_loopback: Vec<u16>,
    /// Allow real Python via the sandbox (REPL + `/api/code/run`).
    pub run_python: bool,
    /// Allow real Node.js via the sandbox.
    pub run_node: bool,
    /// Network for python/node REPL runs (off by default, like every sandbox).
    pub code_network: bool,
    /// Scheduler `script` tasks run inside the sandbox.
    pub scheduler_script: bool,
    /// Network for sandboxed scheduler scripts. On by default because cron
    /// scripts overwhelmingly exist to talk to something.
    pub scheduler_network: bool,
}

impl Default for ExecPolicy {
    fn default() -> Self {
        ExecPolicy {
            exec_shell: false,
            exec_network: true,
            exec_fs_mode: FsMode::Open,
            exec_loopback: Vec::new(),
            run_python: true,
            run_node: true,
            code_network: false,
            scheduler_script: false,
            scheduler_network: true,
        }
    }
}

const KEY: &str = "exec_policy";

pub fn load(db: &Db) -> ExecPolicy {
    db.setting(KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<ExecPolicy>(&s).ok())
        .unwrap_or_default()
}

pub fn save(db: &Db, p: &ExecPolicy) -> Result<ExecPolicy> {
    db.set_setting(KEY, &serde_json::to_string(p)?)?;
    Ok(p.clone())
}

/// The policy in force right now, from the shared engine DB. Falls back to the
/// defaults when the engine is unavailable — which keeps every enforcement
/// site on its legacy path instead of failing the user's action.
pub fn current() -> ExecPolicy {
    match shared_db() {
        Some(db) => load(&db),
        None => ExecPolicy::default(),
    }
}

/// Sandbox rows that back enforced agent-Bash runs are named this way so the
/// UI can label them and `ensure_agent_sandbox` can find them again.
pub const AGENT_SANDBOX_PREFIX: &str = "agent:";

/// Find or create the persistent sandbox row for a chat working directory.
///
/// The row's `workdir` IS the working directory (not a workspace subdir): the
/// point of `exec_shell` is to confine writes to the place the agent already
/// works in. `runner::delete(purge)` never removes files outside
/// `workspaces_dir`, so purging this row can never delete the user's project.
pub fn ensure_agent_sandbox(db: &Db, working_dir: &str, p: &ExecPolicy) -> Result<crate::sandbox::db::Sandbox> {
    let existing = db
        .list_sandboxes()?
        .into_iter()
        .find(|s| s.workdir == working_dir && s.name.starts_with(AGENT_SANDBOX_PREFIX));
    let sb = match existing {
        Some(sb) => sb,
        None => {
            let label = std::path::Path::new(working_dir)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| working_dir.to_string());
            db.create_sandbox(NewSandbox {
                name: format!("{AGENT_SANDBOX_PREFIX}{label}"),
                backend: "direct".into(),
                image: None,
                workdir: working_dir.to_string(),
                network: p.exec_network,
                cpus: 4.0,
                memory_mb: 4096,
                pids_limit: 256,
                timeout_ms: 180_000,
                env: serde_json::json!({}),
                mounts: Vec::new(),
                fs_mode: p.exec_fs_mode,
                ports: crate::sandbox::ports::validate(&[], &[], &p.exec_loopback)
                    .unwrap_or_default(),
            })?
        }
    };
    // The policy is the source of truth for these two knobs — a toggle flipped
    // in settings must reach the next run, not the next sandbox.
    let sb = if sb.network != p.exec_network {
        db.update_limits(&sb.id, None, Some(p.exec_network), None, None, None, None)?
    } else {
        sb
    };
    let sb = if sb.fs_mode != p.exec_fs_mode {
        db.set_fs_mode(&sb.id, p.exec_fs_mode)?
    } else {
        sb
    };
    // Same reasoning as the two above: a port added in settings has to reach the
    // next run, not the next sandbox.
    let wanted = crate::sandbox::ports::validate(&[], &[], &p.exec_loopback).unwrap_or_default();
    let sb = if sb.ports != wanted {
        db.set_ports(&sb.id, &wanted)?
    } else {
        sb
    };
    Ok(sb)
}

/// Run an agent `Bash` command under the OS sandbox, write-jailed to
/// `working_dir`. Returns the recorded run (visible in the sandbox UI).
pub async fn agent_shell(command: &str, working_dir: &str, timeout_ms: u64) -> Result<Run> {
    let db = shared_db().ok_or_else(|| anyhow!("sandbox engine unavailable"))?;
    let p = load(&db);
    let sb = ensure_agent_sandbox(&db, working_dir, &p)?;
    runner::exec(
        &db,
        &sb,
        command,
        Some(timeout_ms as i64),
        BTreeMap::new(),
        "exec",
        None,
        command,
        runner::shell_argv(&sb),
    )
    .await
}

/// Run a python/node/bash snippet in a throwaway sandbox for the REPL and the
/// scheduler. `network` comes from the caller's policy switch.
pub async fn run_once_sandboxed(
    language: &str,
    code: &str,
    network: bool,
    timeout_ms: Option<i64>,
) -> Result<Run> {
    let db = shared_db().ok_or_else(|| anyhow!("sandbox engine unavailable"))?;
    let (run, _sb) = runner::run_once(&db, language, code, None, network, timeout_ms).await?;
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_legacy_paths_and_enable_only_the_new_runtimes() {
        let p = ExecPolicy::default();
        assert!(!p.exec_shell, "agent Bash must not silently change behaviour");
        assert!(!p.scheduler_script, "cron scripts must not silently change behaviour");
        assert!(p.run_python && p.run_node, "the new runtimes ship enabled (always sandboxed)");
        assert!(!p.code_network, "REPL runs start offline");
    }

    #[test]
    fn save_load_round_trips_and_corrupt_rows_fall_back() {
        let db = Db::open_memory().unwrap();
        let mut p = ExecPolicy::default();
        p.exec_shell = true;
        p.exec_fs_mode = FsMode::Strict;
        p.run_node = false;
        save(&db, &p).unwrap();
        let back = load(&db);
        assert!(back.exec_shell);
        assert_eq!(back.exec_fs_mode, FsMode::Strict);
        assert!(!back.run_node);

        db.set_setting(KEY, "{broken").unwrap();
        assert!(!load(&db).exec_shell, "corrupt row must fall back to defaults");
    }

    #[test]
    fn the_agent_shell_reaches_no_local_service_until_one_is_named() {
        // Enforced Bash runs with the network on. Left alone, that would let the
        // agent's shell call SenClaw's own unauthenticated API (and every Space
        // App) on loopback — so the default is an empty list, and a port added
        // in settings must reach the existing sandbox rather than only new ones.
        let db = Db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path().to_string_lossy().to_string();

        let mut p = ExecPolicy::default();
        assert!(p.exec_loopback.is_empty(), "no local service by default");
        let sb = ensure_agent_sandbox(&db, &wd, &p).unwrap();
        assert!(sb.ports.loopback.is_empty());

        p.exec_loopback = vec![3000];
        let sb = ensure_agent_sandbox(&db, &wd, &p).unwrap();
        assert_eq!(sb.ports.loopback, vec![3000], "the same sandbox must pick it up");

        p.exec_loopback.clear();
        let sb = ensure_agent_sandbox(&db, &wd, &p).unwrap();
        assert!(sb.ports.loopback.is_empty(), "removing it must close the door again");
    }

    #[test]
    fn a_partial_row_fills_missing_fields_with_defaults() {
        // Rows written by an older build lack newer fields; `serde(default)`
        // on the struct is what keeps them loading.
        let db = Db::open_memory().unwrap();
        db.set_setting(KEY, r#"{"execShell":true}"#).unwrap();
        let p = load(&db);
        assert!(p.exec_shell);
        assert!(p.exec_network, "missing fields take their defaults");
    }

    #[test]
    fn agent_sandbox_is_created_once_and_tracks_the_policy() {
        let db = Db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path().to_string_lossy().to_string();
        let p = ExecPolicy::default();

        let a = ensure_agent_sandbox(&db, &wd, &p).unwrap();
        assert!(a.name.starts_with(AGENT_SANDBOX_PREFIX));
        assert_eq!(a.workdir, wd);
        assert_eq!(a.fs_mode, FsMode::Open);

        // Same workdir → same row, not a second one.
        let b = ensure_agent_sandbox(&db, &wd, &p).unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(db.list_sandboxes().unwrap().len(), 1);

        // Tightening the policy reaches the existing row.
        let mut tighter = p.clone();
        tighter.exec_fs_mode = FsMode::Strict;
        tighter.exec_network = false;
        let c = ensure_agent_sandbox(&db, &wd, &tighter).unwrap();
        assert_eq!(c.id, a.id);
        assert_eq!(c.fs_mode, FsMode::Strict);
        assert!(!c.network);
    }
}

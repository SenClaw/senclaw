//! Make a Node or Python Space App runnable before its first launch.
//!
//! A native app ships everything it needs. A Node or Python app ships *source*,
//! and the difference between "installed" and "runnable" is a dependency tree
//! that has to come from somewhere. Without this step, `npm start` fails with
//! `Cannot find module` and `python app.py` with `ModuleNotFoundError`, both of
//! which read like the app is broken.
//!
//! So: once per install or update, in the app's own directory, we run the
//! install command — `npm ci` when a lockfile is there, `npm install` when it
//! is not, `pip install -r requirements.txt` for Python — and record what we
//! did in a stamp file. The stamp is keyed on the *content* of the manifests
//! and lockfiles, so an update that changes dependencies re-runs and one that
//! does not, does not.
//!
//! # Why Python gets a virtualenv and Node does not
//!
//! `npm install` writes to `node_modules` inside the app directory: local by
//! construction. `pip install` writes to whichever interpreter it finds, which
//! on most machines is the user's system Python — installing an app's pinned
//! dependencies there is not ours to do, and one app's pin would silently
//! become every app's. So a Python app gets `<app>/.venv`, and its launch runs
//! with that venv first on `PATH`.
//!
//! # Why this runs unconfined
//!
//! Installing dependencies means network access and writes into the app
//! directory. Running it inside the app's own sandbox — which may be declared
//! `network: off` — would fail in a way that looks like the sandbox is broken.
//! The install command comes from the app's manifest, which the pre-install
//! security scan has already seen; the *app* is what gets confined, at launch.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use super::manifest::{Runner, RuntimeSpec};

/// What preparing an app produced.
#[derive(Debug, Clone, Default)]
pub struct Prepared {
    /// Environment the launch must add — the venv on `PATH`, `VIRTUAL_ENV`.
    pub env: Vec<(String, String)>,
    /// Human-readable lines for the runtime log: what ran, or why nothing did.
    pub notes: Vec<String>,
    /// True when an install command actually ran this time.
    pub installed: bool,
}

/// Longest an install may take before we give up on it. `npm ci` on a cold
/// cache is genuinely slow; an hour is not.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// Prepare `app_dir` for `spec`, if its runner needs it. Idempotent and cheap
/// on the common path: a stamp match returns without spawning anything.
pub async fn prepare(app_id: &str, app_dir: &Path, spec: &RuntimeSpec) -> Result<Prepared> {
    match spec.runner {
        Runner::Node => prepare_node(app_id, app_dir, spec).await,
        Runner::Python => prepare_python(app_id, app_dir, spec).await,
        Runner::Binary | Runner::Shell => Ok(Prepared::default()),
    }
}

// ---------------------------------------------------------------------------
// Node
// ---------------------------------------------------------------------------

async fn prepare_node(app_id: &str, app_dir: &Path, spec: &RuntimeSpec) -> Result<Prepared> {
    let mut out = Prepared::default();
    let pkg = app_dir.join("package.json");
    if !pkg.is_file() && spec.install.is_none() {
        out.notes.push("[prepare] node app with no package.json — nothing to install".into());
        return Ok(out);
    }

    let cmd = match &spec.install {
        Some(c) => c.clone(),
        None => {
            // `npm ci` needs a lockfile *and* refuses to run without one, so
            // the presence of the lockfile is the whole decision.
            if app_dir.join("package-lock.json").is_file() {
                "npm ci --omit=dev".to_string()
            } else if app_dir.join("pnpm-lock.yaml").is_file() {
                "pnpm install --prod --frozen-lockfile".to_string()
            } else if app_dir.join("yarn.lock").is_file() {
                "yarn install --production --frozen-lockfile".to_string()
            } else {
                "npm install --omit=dev".to_string()
            }
        }
    };

    let key = stamp_key(
        app_dir,
        &cmd,
        &["package.json", "package-lock.json", "pnpm-lock.yaml", "yarn.lock"],
    );
    if stamp_matches(app_dir, &key) && app_dir.join("node_modules").is_dir() {
        out.notes.push("[prepare] node dependencies already installed".into());
        return Ok(out);
    }

    run_install(app_id, app_dir, &cmd, &[]).await?;
    write_stamp(app_dir, &key);
    out.installed = true;
    out.notes.push(format!("[prepare] ran `{cmd}`"));
    Ok(out)
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

async fn prepare_python(app_id: &str, app_dir: &Path, spec: &RuntimeSpec) -> Result<Prepared> {
    let mut out = Prepared::default();
    let reqs = app_dir.join("requirements.txt");
    let has_work = reqs.is_file()
        || spec.install.is_some()
        || app_dir.join("pyproject.toml").is_file();

    if !spec.venv {
        if let Some(cmd) = &spec.install {
            let key = stamp_key(app_dir, cmd, &["requirements.txt", "pyproject.toml"]);
            if !stamp_matches(app_dir, &key) {
                run_install(app_id, app_dir, cmd, &[]).await?;
                write_stamp(app_dir, &key);
                out.installed = true;
                out.notes.push(format!("[prepare] ran `{cmd}` (no venv: runtime.venv=false)"));
            }
        }
        return Ok(out);
    }

    let venv = app_dir.join(".venv");
    let bin_dir = venv_bin(&venv);
    let python = bin_dir.join(if cfg!(windows) { "python.exe" } else { "python" });

    if !python.is_file() {
        let base = super::requirements::which("python3")
            .await
            .or(super::requirements::which("python").await)
            .ok_or_else(|| {
                anyhow!(
                    "python is required to run '{app_id}' but is not on the daemon's PATH — \
                     install Python 3 and restart SenClaw"
                )
            })?;
        run_install(app_id, app_dir, &format!("{base} -m venv .venv"), &[]).await?;
        out.notes.push(format!("[prepare] created .venv with {base}"));
    }
    if !python.is_file() {
        return Err(anyhow!(
            "creating a virtualenv for '{app_id}' produced no interpreter at {}",
            python.display()
        ));
    }

    // The launch environment: the venv first on PATH, so `python`, `pip` and
    // any console-script entry point the app installed resolve to it.
    let path = match std::env::var("PATH") {
        Ok(p) => format!("{}{}{}", bin_dir.display(), path_sep(), p),
        Err(_) => bin_dir.display().to_string(),
    };
    let env = vec![
        ("VIRTUAL_ENV".to_string(), venv.display().to_string()),
        ("PATH".to_string(), path),
        // Otherwise a stray `PYTHONHOME` in the daemon's environment makes the
        // venv interpreter load the wrong standard library.
        ("PYTHONHOME".to_string(), String::new()),
        ("PYTHONUNBUFFERED".to_string(), "1".to_string()),
    ];
    out.env = env.clone();

    if !has_work {
        out.notes.push("[prepare] venv ready; no dependencies declared".into());
        return Ok(out);
    }

    let cmd = match &spec.install {
        Some(c) => c.clone(),
        None if reqs.is_file() => "pip install -r requirements.txt".to_string(),
        None => "pip install .".to_string(),
    };
    let key = stamp_key(app_dir, &cmd, &["requirements.txt", "pyproject.toml", "poetry.lock"]);
    if stamp_matches(app_dir, &key) {
        out.notes.push("[prepare] python dependencies already installed".into());
        return Ok(out);
    }
    run_install(app_id, app_dir, &cmd, &env).await?;
    write_stamp(app_dir, &key);
    out.installed = true;
    out.notes.push(format!("[prepare] ran `{cmd}` in .venv"));
    Ok(out)
}

fn path_sep() -> &'static str {
    if cfg!(windows) {
        ";"
    } else {
        ":"
    }
}

/// `<venv>/bin` on unix, `<venv>/Scripts` on Windows.
pub fn venv_bin(venv: &Path) -> PathBuf {
    venv.join(if cfg!(windows) { "Scripts" } else { "bin" })
}

// ---------------------------------------------------------------------------
// Running an install command
// ---------------------------------------------------------------------------

async fn run_install(
    app_id: &str,
    app_dir: &Path,
    cmd: &str,
    env: &[(String, String)],
) -> Result<()> {
    tracing::info!("[space-prepare] {app_id}: {cmd}");
    let log_path = app_dir.join(".senclaw").join("runtime.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut command = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-lc").arg(cmd);
        c
    };
    command
        .current_dir(app_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        if v.is_empty() {
            command.env_remove(k);
        } else {
            command.env(k, v);
        }
    }

    let out = tokio::time::timeout(INSTALL_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            anyhow!("install step for '{app_id}' timed out after {}s: {cmd}", INSTALL_TIMEOUT.as_secs())
        })?
        .with_context(|| format!("run install step for '{app_id}': {cmd}"))?;

    // The output belongs in the app's runtime log — this is exactly where a
    // failing install has to be readable from, and it is the file the Web UI's
    // log view already shows.
    let text = format!(
        "\n===== {} prepare: {cmd} =====\n{}{}",
        chrono::Utc::now().to_rfc3339(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = f.write_all(text.as_bytes());
    }

    if !out.status.success() {
        let tail: String = String::from_utf8_lossy(&out.stderr)
            .lines()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(anyhow!(
            "install step failed for '{app_id}' (`{cmd}`):\n{tail}\nFull output: {}",
            log_path.display()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Stamps — "we already did this, for exactly these inputs"
// ---------------------------------------------------------------------------

fn stamp_path(app_dir: &Path) -> PathBuf {
    app_dir.join(".senclaw").join("prepare.stamp")
}

/// A fingerprint of the install command plus the files that decide what it
/// installs. Content, not mtime: extracting a zip rewrites every mtime, and a
/// stamp that invalidates on every update would reinstall on every update.
pub fn stamp_key(app_dir: &Path, cmd: &str, files: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(cmd.as_bytes());
    for f in files {
        h.update(f.as_bytes());
        if let Ok(bytes) = std::fs::read(app_dir.join(f)) {
            h.update(&bytes);
        } else {
            h.update(b"<absent>");
        }
    }
    hex::encode(h.finalize())
}

fn stamp_matches(app_dir: &Path, key: &str) -> bool {
    std::fs::read_to_string(stamp_path(app_dir))
        .map(|s| s.trim() == key)
        .unwrap_or(false)
}

fn write_stamp(app_dir: &Path, key: &str) {
    let path = stamp_path(app_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, key);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(v: serde_json::Value) -> RuntimeSpec {
        RuntimeSpec::parse(&v)
    }

    #[tokio::test]
    async fn a_native_app_is_never_prepared() {
        let dir = tempfile::tempdir().unwrap();
        let s = spec(json!({"runtime": {"kind": "server", "start": "./crm"}}));
        let out = prepare("crm", dir.path(), &s).await.unwrap();
        assert!(!out.installed && out.env.is_empty() && out.notes.is_empty());
    }

    #[tokio::test]
    async fn a_node_app_with_nothing_to_install_says_so_instead_of_running_npm() {
        let dir = tempfile::tempdir().unwrap();
        let s = spec(json!({"runtime": {"kind": "server", "start": "node server.js"}}));
        let out = prepare("demo", dir.path(), &s).await.unwrap();
        assert!(!out.installed);
        assert!(out.notes[0].contains("no package.json"), "{:?}", out.notes);
    }

    #[test]
    fn the_stamp_follows_content_not_timestamps() {
        // Extracting an update rewrites every mtime; if the stamp keyed on that,
        // every update would reinstall. It must key on what is in the files.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"a"}"#).unwrap();
        let a = stamp_key(dir.path(), "npm ci", &["package.json"]);
        // Rewrite identical content with a new mtime.
        std::fs::write(dir.path().join("package.json"), r#"{"name":"a"}"#).unwrap();
        assert_eq!(a, stamp_key(dir.path(), "npm ci", &["package.json"]));
        // Change the dependencies → a different key.
        std::fs::write(dir.path().join("package.json"), r#"{"name":"b"}"#).unwrap();
        assert_ne!(a, stamp_key(dir.path(), "npm ci", &["package.json"]));
        // Change the command → also a different key.
        assert_ne!(
            stamp_key(dir.path(), "npm ci", &["package.json"]),
            stamp_key(dir.path(), "npm install", &["package.json"])
        );
    }

    #[test]
    fn a_stamp_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!stamp_matches(dir.path(), "abc"));
        write_stamp(dir.path(), "abc");
        assert!(stamp_matches(dir.path(), "abc"));
        assert!(!stamp_matches(dir.path(), "def"));
    }

    #[test]
    fn the_venv_bin_dir_is_platform_correct() {
        let v = venv_bin(Path::new("/app/.venv"));
        if cfg!(windows) {
            assert!(v.ends_with("Scripts"));
        } else {
            assert!(v.ends_with("bin"));
        }
    }

    #[tokio::test]
    async fn an_install_failure_names_the_command_and_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let err = run_install("demo", dir.path(), "exit 3", &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exit 3"), "{err}");
        assert!(err.contains("runtime.log"), "{err}");
    }

    /// The real path, when the machine has a Python: a venv is created, its
    /// interpreter exists, and the launch env points at it.
    #[tokio::test]
    async fn a_python_app_gets_a_working_venv() {
        if super::super::requirements::which("python3").await.is_none() {
            return; // no python3 on this machine
        }
        let dir = tempfile::tempdir().unwrap();
        let s = spec(json!({"runtime": {"kind": "server", "start": "python3 app.py"}}));
        let out = prepare("demo", dir.path(), &s).await.unwrap();
        let bin = venv_bin(&dir.path().join(".venv"));
        assert!(bin.join(if cfg!(windows) { "python.exe" } else { "python" }).is_file());
        let path = out.env.iter().find(|(k, _)| k == "PATH").expect("PATH in env").1.clone();
        assert!(path.starts_with(&bin.display().to_string()), "venv must come first: {path}");
        assert!(out.env.iter().any(|(k, v)| k == "VIRTUAL_ENV" && !v.is_empty()));
    }
}

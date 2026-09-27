//! `docker` backend — the script runs inside a container.
//!
//! The container is long-lived (`sleep infinity` as PID 1) and every run is a
//! `docker exec` into it. That is what makes a sandbox feel like a machine:
//! `pip install` in one call is still there in the next, because the container
//! filesystem persists between execs.
//!
//! The sandbox's directory on the host is bind-mounted at `/work`, so files
//! survive even a container that is destroyed and recreated, and the UI's file
//! browser reads them straight off the host without going through Docker.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::io::AsyncWriteExt;

use super::{build_env, clamp, ExecSpec, Outcome};
use crate::sandbox::config;
use crate::sandbox::db::Sandbox;

/// Mount point of the sandbox directory inside the container.
pub const WORK: &str = "/work";

/// Pulling an image is slow and size-dependent; it gets its own generous
/// budget rather than the per-run timeout.
const PULL_TIMEOUT: Duration = Duration::from_secs(600);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

fn container_name(sandbox_id: &str) -> String {
    // Docker names allow [a-zA-Z0-9][a-zA-Z0-9_.-]*; a UUID qualifies.
    format!("senclaw-sbx-{sandbox_id}")
}

/// Arguments for `docker run`. Pure, so the limits can be asserted on — a
/// dropped `--memory` is invisible until a runaway allocation takes the host
/// down with it.
pub fn run_args(sb: &Sandbox, image: &str) -> Vec<String> {
    let mem = format!("{}m", sb.memory_mb.max(64));
    let mut a: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        container_name(&sb.id),
        // No network at all unless the sandbox asked for it. `none` is a real
        // network namespace with only a loopback — not a firewall rule that
        // something inside could route around.
        "--network".into(),
        // `--network none` cannot publish anything, so an opened port forces a
        // network onto the container. `ports::note_for` tells the user.
        if sb.network || sb.ports.wants_network() {
            "bridge".into()
        } else {
            "none".into()
        },
        "--memory".into(),
        mem.clone(),
        // Without a swap cap equal to the memory cap, the container can swap
        // past `--memory` and the limit means much less than it appears to.
        "--memory-swap".into(),
        mem,
        "--cpus".into(),
        format!("{:.2}", sb.cpus.clamp(0.1, 32.0)),
        "--pids-limit".into(),
        sb.pids_limit.clamp(16, 8192).to_string(),
        // Drop everything the workload does not need. A code sandbox never
        // needs to change capabilities, and `no-new-privileges` stops a setuid
        // binary inside the image from being a way back up.
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "-v".into(),
        format!("{}:{}", sb.workdir, WORK),
        "-w".into(),
        WORK.into(),
    ];

    a.extend(crate::sandbox::ports::docker_publish_args(&sb.ports));

    // Host folders the user asked for, mounted under the sandbox root so the
    // path inside the container matches the path the direct backend uses.
    for m in &sb.mounts {
        a.push("-v".into());
        a.push(format!(
            "{}:{}/{}{}",
            m.source,
            WORK,
            m.target,
            if m.read_only { ":ro" } else { "" }
        ));
    }

    a.extend([
        // Overriding the entrypoint matters: many images (python, node) set one
        // that would swallow the `sleep` and exit immediately.
        "--entrypoint".into(),
        "sh".into(),
        image.to_string(),
        "-c".into(),
        "sleep infinity".into(),
    ]);
    a
}

/// Arguments for a `docker exec` that reads its script from stdin.
pub fn exec_args(container: &str, env: &[(String, String)]) -> Vec<String> {
    let mut a = vec!["exec".to_string(), "-i".to_string(), "-w".to_string(), WORK.to_string()];
    for (k, v) in env {
        a.push("-e".into());
        a.push(format!("{k}={v}"));
    }
    a.push(container.to_string());
    a.push("sh".into());
    a.push("-s".into());
    a
}

// ── lifecycle ───────────────────────────────────────────────────────────────

/// True when the image is already local. A missing image is not an error here —
/// the caller decides whether to pull.
pub async fn has_image(image: &str) -> bool {
    control(&["image", "inspect", image], CONTROL_TIMEOUT)
        .await
        .is_ok()
}

pub async fn pull_image(image: &str) -> Result<String> {
    control(&["pull", image], PULL_TIMEOUT).await
}

/// Start (or adopt) the container for this sandbox, returning its id.
pub async fn start(sb: &Sandbox) -> Result<String> {
    let image = sb
        .image
        .clone()
        .unwrap_or_else(config::default_image);
    let name = container_name(&sb.id);

    // A container from a previous app run may still exist. Reuse it rather than
    // failing on the name clash — restarting the app must not orphan sandboxes.
    if let Ok(state) = control(&["inspect", "-f", "{{.State.Running}}", &name], CONTROL_TIMEOUT).await
    {
        if state.trim() == "true" {
            return Ok(name);
        }
        // Exists but stopped. Its limits may be stale relative to the DB row, so
        // it is removed and recreated instead of merely started.
        let _ = control(&["rm", "-f", &name], CONTROL_TIMEOUT).await;
    }

    if !has_image(&image).await {
        pull_image(&image)
            .await
            .map_err(|e| anyhow!("cannot pull image `{image}`: {e}"))?;
    }

    std::fs::create_dir_all(Path::new(&sb.workdir).join(".tmp"))
        .map_err(|e| anyhow!("cannot create sandbox directory: {e}"))?;

    let args = run_args(sb, &image);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    control(&refs, CONTROL_TIMEOUT)
        .await
        .map_err(|e| anyhow!("cannot start the container: {e}"))?;
    Ok(name)
}

pub async fn stop(sb: &Sandbox) -> Result<()> {
    let name = container_name(&sb.id);
    control(&["rm", "-f", &name], CONTROL_TIMEOUT).await?;
    Ok(())
}

/// Restart the container. This is the timeout remedy: killing the `docker exec`
/// client leaves the process running *inside* the container, so a run that hits
/// its deadline would otherwise keep burning CPU forever, invisible to the app.
/// Restarting kills it; the workdir is a host bind mount and installed packages
/// live in the container's writable layer, so neither is lost.
pub async fn restart(sb: &Sandbox) -> Result<()> {
    let name = container_name(&sb.id);
    control(&["restart", "-t", "1", &name], CONTROL_TIMEOUT).await?;
    Ok(())
}

pub async fn is_running(sb: &Sandbox) -> bool {
    matches!(
        control(&["inspect", "-f", "{{.State.Running}}", &container_name(&sb.id)], CONTROL_TIMEOUT).await,
        Ok(s) if s.trim() == "true"
    )
}

// ── exec ────────────────────────────────────────────────────────────────────

pub async fn exec(sb: &Sandbox, spec: &ExecSpec) -> Outcome {
    let start = Instant::now();
    let name = container_name(&sb.id);

    // `HOME` is the in-container mount point, not the host path — the host path
    // does not exist inside the container, and tools that write to `$HOME`
    // would fail in a way that reads as a broken image.
    let env = build_env(sb, &spec.extra_env, WORK);

    let args = exec_args(&name, &env);
    let mut cmd = tokio::process::Command::new(config::docker_bin());
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return failed(format!("cannot invoke docker: {e}"), start),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(spec.script.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    match tokio::time::timeout(
        Duration::from_millis(spec.timeout_ms),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(out)) => {
            let (stdout, t1) = clamp(String::from_utf8_lossy(&out.stdout).to_string());
            let (stderr, t2) = clamp(String::from_utf8_lossy(&out.stderr).to_string());
            Outcome {
                exit_code: out.status.code(),
                stdout,
                stderr,
                truncated: t1 || t2,
                timed_out: false,
                duration_ms: start.elapsed().as_millis() as i64,
                isolation: "container".into(),
            }
        }
        Ok(Err(e)) => failed(format!("error while waiting for docker exec: {e}"), start),
        Err(_) => {
            // See `restart` — the client is dead, the workload is not.
            let restarted = restart(sb).await.is_ok();
            let note = if restarted {
                "the container was restarted to stop it."
            } else {
                "the container could NOT be restarted — the process may still be running."
            };
            Outcome {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("Timed out after {} ms — {note}", spec.timeout_ms),
                truncated: false,
                timed_out: true,
                duration_ms: start.elapsed().as_millis() as i64,
                isolation: "container".into(),
            }
        }
    }
}

/// Run a docker control command, capturing stdout and surfacing stderr on
/// failure. Always time-boxed: a wedged daemon must not wedge the app.
async fn control(args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = tokio::process::Command::new(config::docker_bin());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd.spawn().map_err(|e| anyhow!("{e}"))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => Err(anyhow!(
            "docker did not answer within {}s",
            timeout.as_secs()
        )),
        Ok(Err(e)) => Err(anyhow!("{e}")),
        Ok(Ok(out)) => {
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).to_string())
            } else {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Err(anyhow!(if err.is_empty() {
                    "docker failed without a message".to_string()
                } else {
                    err
                }))
            }
        }
    }
}

fn failed(msg: String, start: Instant) -> Outcome {
    Outcome {
        exit_code: None,
        stdout: String::new(),
        stderr: msg,
        truncated: false,
        timed_out: false,
        duration_ms: start.elapsed().as_millis() as i64,
        isolation: "container".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sb(network: bool, mem: i64, cpus: f64) -> Sandbox {
        Sandbox {
            id: "abc".into(),
            name: "n".into(),
            backend: "docker".into(),
            image: Some("python:3.12-slim".into()),
            workdir: "/host/ws/abc".into(),
            network,
            cpus,
            memory_mb: mem,
            pids_limit: 256,
            timeout_ms: 1000,
            env: json!({}),
            mounts: Vec::new(),
            fs_mode: crate::sandbox::fsmode::FsMode::Strict,
            trace_enabled: false,
            ports: Default::default(),
            status: "stopped".into(),
            container_id: None,
            last_error: None,
            created_at: 0,
            updated_at: 0,
            last_used_at: None,
        }
    }

    #[test]
    fn network_is_off_by_default_and_on_only_when_asked() {
        let off = run_args(&sb(false, 512, 1.0), "img").join(" ");
        assert!(off.contains("--network none"));
        let on = run_args(&sb(true, 512, 1.0), "img").join(" ");
        assert!(on.contains("--network bridge"));
    }

    #[test]
    fn memory_swap_matches_memory_so_the_cap_is_real() {
        let a = run_args(&sb(false, 256, 1.0), "img");
        let mem = arg_after(&a, "--memory").unwrap();
        let swap = arg_after(&a, "--memory-swap").unwrap();
        assert_eq!(mem, "256m");
        assert_eq!(swap, mem, "swap above the memory cap defeats the limit");
    }

    #[test]
    fn limits_are_clamped_not_trusted() {
        let a = run_args(&sb(false, 1, 0.0), "img");
        assert_eq!(arg_after(&a, "--memory").unwrap(), "64m");
        assert_eq!(arg_after(&a, "--cpus").unwrap(), "0.10");
    }

    #[test]
    fn privileges_are_dropped() {
        let a = run_args(&sb(false, 512, 1.0), "img").join(" ");
        assert!(a.contains("--cap-drop ALL"));
        assert!(a.contains("--security-opt no-new-privileges"));
    }

    #[test]
    fn entrypoint_is_overridden_so_the_container_stays_up() {
        let a = run_args(&sb(false, 512, 1.0), "img").join(" ");
        assert!(a.contains("--entrypoint sh"), "an image entrypoint would swallow the sleep");
        assert!(a.ends_with("sleep infinity"));
    }

    #[test]
    fn workdir_is_bind_mounted_at_the_documented_path() {
        let a = run_args(&sb(false, 512, 1.0), "img").join(" ");
        assert!(a.contains("-v /host/ws/abc:/work"));
        assert!(a.contains("-w /work"));
    }

    #[test]
    fn exec_reads_the_script_from_stdin() {
        let a = exec_args("c1", &[]);
        assert_eq!(a.last().map(String::as_str), Some("-s"));
        assert!(a.contains(&"-i".to_string()), "stdin must stay open");
        assert!(!a.iter().any(|x| x == "-c"));
    }

    #[test]
    fn exec_passes_env_as_separate_flags_not_a_shell_assignment() {
        let env = vec![("K".to_string(), "a b;c".to_string())];
        let a = exec_args("c1", &env);
        // The value keeps its spaces and semicolon as one argv entry, so it can
        // never be reinterpreted as shell syntax.
        assert!(a.contains(&"K=a b;c".to_string()));
    }

    fn arg_after(args: &[String], flag: &str) -> Option<String> {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
    }
}

//! Starting the agent's shell: unconfined (`bash -c`) or inside the OS sandbox,
//! as Plugins → Sandbox → "exec" decides. Every shell starts as a session leader
//! so one signal reaches whatever it started.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

use super::capture::{wait_shell, Capture, Held, Waited};

/// Bytes kept per stream of an unconfined foreground run; `Bash` cuts the
/// combined text to 30 000 characters, so this only bounds memory.
const STREAM_CAP: usize = 256 * 1024;

pub struct RawRun {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    /// The shell's process group, when it can still hold survivors.
    pub group: Option<u32>,
    pub held: Option<Held>,
}

fn raw_command(command: &str, cwd: &str) -> Command {
    let mut c = Command::new("bash");
    c.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own group: a timeout can end the whole family, and what a command
    // leaves running can be found by the group it was born in.
    #[cfg(unix)]
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    c
}

/// Run `command` in the foreground without a sandbox.
pub async fn run_raw(command: &str, cwd: &str, timeout_ms: u64) -> Result<RawRun, String> {
    let mut child = raw_command(command, cwd)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to execute command: {e}"))?;
    let pid = child.id();
    let out = Capture::spawn(child.stdout.take(), STREAM_CAP);
    let err = Capture::spawn(child.stderr.take(), STREAM_CAP);
    let text = |c: &Capture| String::from_utf8_lossy(&c.snapshot().0).into_owned();
    match wait_shell(&mut child, Duration::from_millis(timeout_ms), &out, &err, true).await {
        Waited::Exited { status, held } => Ok(RawRun {
            stdout: text(&out),
            stderr: text(&err),
            exit_code: status.code().unwrap_or(-1),
            timed_out: false,
            group: if cfg!(unix) { pid } else { None },
            held,
        }),
        Waited::TimedOut => {
            super::proc::kill_group_now(pid);
            Ok(RawRun {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: -1,
                timed_out: true,
                group: None,
                held: None,
            })
        }
        Waited::Failed(e) => Err(format!("Command execution failed: {e}")),
    }
}

/// Start `command` for a background task: piped output, nothing waited for.
/// Returns the child and what confines it (empty when nothing does).
pub async fn spawn(command: &str, cwd: &str) -> Result<(Child, String), String> {
    if crate::sandbox::policy::current().shell_sandboxed() {
        return crate::sandbox::policy::agent_shell_spawn(command, cwd).await.map_err(|e| {
            format!(
                "Sandboxed execution failed: {e}. \
                 (Bash runs inside the OS sandbox because `exec` enforcement is on in Plugins → Sandbox.)"
            )
        });
    }
    let child = raw_command(command, cwd)
        .spawn()
        .map_err(|e| format!("Failed to execute command: {e}"))?;
    Ok((child, String::new()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_plain_command_returns_its_output_and_leaves_nothing() {
        let run = run_raw("echo hi; echo oops >&2; exit 3", "/tmp", 10_000).await.unwrap();
        assert_eq!(run.stdout.trim(), "hi");
        assert_eq!(run.stderr.trim(), "oops");
        assert_eq!(run.exit_code, 3);
        assert!(run.held.is_none());
        assert!(!super::super::proc::group_alive(run.group.unwrap()));
    }

    #[tokio::test]
    async fn a_background_child_that_keeps_the_pipes_does_not_hold_the_call_up() {
        // Without the shell-exit rule this blocks for the whole timeout.
        let started = std::time::Instant::now();
        let run = run_raw("sleep 30 & echo started", "/tmp", 20_000).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "waited for the pipes");
        assert_eq!(run.stdout.trim(), "started");
        assert!(run.held.is_some(), "the sleep still holds the pipes");
        let g = run.group.unwrap();
        assert!(super::super::proc::group_alive(g));
        super::super::proc::kill_group_now(Some(g));
    }

    #[tokio::test]
    async fn a_timeout_ends_the_whole_group() {
        let run = run_raw("sleep 30 & sleep 30", "/tmp", 300).await.unwrap();
        assert!(run.timed_out);
    }
}

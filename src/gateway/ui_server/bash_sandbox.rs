//! Sandboxed Bash via **brush** (a pure-Rust, bash-compatible shell) — "rust-bash".
//!
//! Scripts run through `brush_core::Shell`, configured as a sandbox:
//!   - **No environment inheritance** + **empty `PATH`** → external programs
//!     referenced by bare name (`ls`, `curl`, `rm`, …) aren't found. Shell
//!     builtins and shell logic still work. The `exec`/`command`/`enable`
//!     builtins are removed.
//!   - **Temp working directory**, removed after the run.
//!   - **No output-redirection overwrite** of existing regular files.
//!   - **Output cap** + **hard wall-clock timeout**.
//!
//! brush has no cooperative cancellation, so a CPU-bound script (`while :; do
//! :; done`) can't be interrupted in-process. To guarantee the timeout, the
//! brush sandbox runs in a **child process** (`senclaw brush-sandbox <dir>`)
//! that the parent **kills** when the deadline passes. The child is still the
//! pure-Rust brush sandbox — the subprocess only exists to make the kill
//! reliable.
//!
//! Caveat: this is process- + shell-level isolation, not an OS jail — a script
//! invoking a binary by *absolute path* could still reach it. For hard
//! isolation of untrusted input, an OS-level sandbox is still required.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use brush_builtins::{default_builtins, BuiltinSet};
use brush_core::openfiles::OpenFile;
use brush_core::{Shell, ShellVariable, SourceInfo};
use serde_json::json;
use tokio::io::AsyncWriteExt;

const MAX_TIMEOUT_MS: u64 = 60_000;
const MAX_OUTPUT: usize = 100_000;

fn clamp(mut s: String) -> String {
    if s.len() > MAX_OUTPUT {
        s.truncate(MAX_OUTPUT);
        s.push_str("\n…[truncated]");
    }
    s
}

// ── Parent side: spawn the child, enforce the timeout by killing it ──────────

/// Run a Bash snippet in the brush sandbox (out-of-process so the timeout is
/// enforceable). Returns the common REPL outcome shape: stdout → `result`,
/// stderr lines → `logs`, non-zero exit / timeout → `error`.
pub async fn run(code: String, timeout_ms: u64) -> serde_json::Value {
    let start = Instant::now();
    let timeout_ms = timeout_ms.clamp(1, MAX_TIMEOUT_MS);

    let dir = std::env::temp_dir().join(format!("senclaw-brush-{}", uuid::Uuid::new_v4()));
    let out_path = dir.join("stdout");
    let err_path = dir.join("stderr");
    if std::fs::create_dir_all(&dir).is_err() {
        return err_outcome("sandbox setup failed", start.elapsed().as_millis());
    }
    // Pre-create so we can always read them back, even on an early kill.
    let _ = std::fs::File::create(&out_path);
    let _ = std::fs::File::create(&err_path);

    // `SENCLAW_BIN` overrides the child executable (same convention as the MCP
    // servers); falls back to the running binary.
    let exe = match std::env::var("SENCLAW_BIN")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::current_exe())
    {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return err_outcome(
                &format!("locate senclaw bin: {e}"),
                start.elapsed().as_millis(),
            );
        }
    };

    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("brush-sandbox")
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return err_outcome(&format!("spawn sandbox: {e}"), start.elapsed().as_millis());
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(code.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    let mut timed_out = false;
    let mut exit_code: i32 = 0;
    match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
        Ok(Ok(status)) => exit_code = status.code().unwrap_or(127),
        Ok(Err(_)) => exit_code = 127,
        Err(_) => {
            timed_out = true;
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
    let duration_ms = start.elapsed().as_millis();

    let stdout = clamp(std::fs::read_to_string(&out_path).unwrap_or_default());
    let stderr = clamp(std::fs::read_to_string(&err_path).unwrap_or_default());
    let _ = std::fs::remove_dir_all(&dir);
    let logs: Vec<String> = stderr.lines().map(str::to_string).collect();

    if timed_out {
        return json!({ "ok": false,
            "error": format!("execution timed out after {timeout_ms}ms (killed)"),
            "result": stdout, "logs": logs, "timed_out": true, "duration_ms": duration_ms });
    }
    let ok = exit_code == 0;
    json!({
        "ok": ok,
        "result": stdout,
        "result_type": format!("exit {exit_code}"),
        "exit_code": exit_code,
        "logs": logs,
        "error": if ok { serde_json::Value::Null }
                 else { serde_json::Value::String(format!("exited with code {exit_code}")) },
        "timed_out": false,
        "duration_ms": duration_ms,
    })
}

fn err_outcome(msg: &str, duration_ms: u128) -> serde_json::Value {
    json!({ "ok": false, "error": msg, "logs": [], "timed_out": false, "duration_ms": duration_ms })
}

// ── Child side: the actual brush sandbox (CLI `senclaw brush-sandbox <dir>`) ──

/// Entry point for the `brush-sandbox <dir>` subcommand. Reads the script from
/// stdin, runs it in the brush sandbox writing the script's stdout/stderr into
/// `<dir>/stdout` and `<dir>/stderr`, and exits with the script's exit code.
/// Never returns. Async because it is dispatched from within `#[tokio::main]`.
pub async fn child_main(dir: &Path) -> ! {
    let mut code = String::new();
    let _ = std::io::stdin().read_to_string(&mut code);
    let exit = run_brush(&code, dir).await;
    std::process::exit(exit as i32);
}

/// Build the brush sandbox, run `code`, write the script's output to
/// `<dir>/stdout` + `<dir>/stderr`, and return the script's exit code.
async fn run_brush(code: &str, dir: &Path) -> u8 {
    let out_file = match std::fs::File::create(dir.join("stdout")) {
        Ok(f) => f,
        Err(_) => return 127,
    };
    let mut err_file = match std::fs::File::create(dir.join("stderr")) {
        Ok(f) => f,
        Err(_) => return 127,
    };

    let mut fds: HashMap<i32, OpenFile> = HashMap::new();
    match brush_core::openfiles::null() {
        Ok(n) => {
            fds.insert(0, n);
        }
        Err(_) => return 127,
    }
    fds.insert(1, OpenFile::from(out_file));
    if let Ok(err_clone) = err_file.try_clone() {
        fds.insert(2, OpenFile::from(err_clone));
    }

    // Full bash builtins minus the external-execution escape hatches.
    let mut builtins = default_builtins(BuiltinSet::BashMode);
    for name in ["exec", "command", "enable"] {
        builtins.remove(name);
    }

    let build = async {
        let mut shell = Shell::builder()
            .working_dir(dir.to_path_buf())
            .do_not_inherit_env(true)
            .disallow_overwriting_regular_files_via_output_redirection(true)
            .interactive(false)
            .no_editing(true)
            .read_commands_from_stdin(false)
            .builtins(builtins)
            .fds(fds)
            .build()
            .await
            .map_err(|e| e.to_string())?;
        shell
            .set_env_global("PATH", ShellVariable::new(""))
            .map_err(|e| e.to_string())?;
        let params = shell.default_exec_params();
        let result = shell
            .run_string(code.to_string(), &SourceInfo::from("sandbox"), &params)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<u8, String>(u8::from(result.exit_code))
    };

    match build.await {
        Ok(code) => code,
        Err(e) => {
            use std::io::Write;
            let _ = writeln!(err_file, "brush sandbox error: {e}");
            127
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run brush in-process (no subprocess) and return (exit_code, stdout, stderr).
    /// Only safe for non-looping scripts — the in-process path has no timeout.
    async fn exec(code: &str) -> (u8, String, String) {
        let dir = std::env::temp_dir().join(format!("brush-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let exit = run_brush(code, &dir).await;
        let out = std::fs::read_to_string(dir.join("stdout")).unwrap_or_default();
        let err = std::fs::read_to_string(dir.join("stderr")).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        (exit, out, err)
    }

    #[tokio::test]
    async fn builtins_run_and_capture_output() {
        let (exit, out, err) = exec("echo hello; echo oops 1>&2").await;
        assert_eq!(exit, 0);
        assert!(out.contains("hello"), "stdout: {out}");
        assert!(err.contains("oops"), "stderr: {err}");
    }

    #[tokio::test]
    async fn shell_logic_arithmetic_and_loops() {
        let (exit, out, _) = exec("s=0; for i in 1 2 3 4; do s=$((s+i)); done; echo $s").await;
        assert_eq!(exit, 0);
        assert!(out.contains("10"), "stdout: {out}");
    }

    #[tokio::test]
    async fn external_programs_are_blocked() {
        // `curl` is external; with empty PATH it must not resolve (non-zero exit).
        let (exit, _, _) = exec("curl https://example.com").await;
        assert_ne!(exit, 0, "external command should have failed");
    }

    #[tokio::test]
    async fn nonzero_exit_propagates() {
        let (exit, _, _) = exec("exit 3").await;
        assert_eq!(exit, 3);
    }
}

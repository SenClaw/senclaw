//! Claude Code cowork: SenClaw dispatches work to the user's own Claude Code
//! CLI, verifies the result itself, and lets Claude Code call back into
//! SenClaw's tools over MCP. See `docs/claude-code-cowork.md`.
//!
//! - [`worker`] — runs a dispatched item (kanban card assignee `claude-code`).
//! - [`hooks`] — the Stop hook that gates finishing on the verify command.
//! - [`mcp_bridge`] — `/mcp/{core|kanban}` Streamable HTTP for `claude mcp add`.
//! - [`api`] — `/api/claude-code/*` status, runs and settings.

pub mod api;
pub mod hooks;
pub mod mcp_bridge;
pub mod registry;
pub mod runner;
pub mod senclaw_mcp;
pub mod settings;
pub mod verify;
pub mod worker;

pub use worker::ClaudeCodeWorker;

/// `claude --version` output, or `None` when the binary cannot run.
pub async fn binary_version(binary: &str) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(binary)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// SIGKILL a whole process group (the child was spawned with
/// `process_group(0)`): Bash commands, dev servers and MCP children a session
/// started die with it instead of outliving the run.
pub(crate) fn kill_group(pgid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pgid {
        // SAFETY: plain syscall on a pid we spawned; a stale group is ESRCH.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

/// Read `r` to EOF keeping only the last `cap` bytes — bounded memory no
/// matter how much a command prints.
pub(crate) async fn read_tail<R: tokio::io::AsyncRead + Unpin>(mut r: R, cap: usize) -> String {
    use tokio::io::AsyncReadExt;
    let mut buf: std::collections::VecDeque<u8> = std::collections::VecDeque::with_capacity(cap.min(64 * 1024));
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend(&chunk[..n]);
                let over = buf.len().saturating_sub(cap);
                buf.drain(..over);
            }
        }
    }
    String::from_utf8_lossy(buf.make_contiguous()).into_owned()
}

/// Loopback base URL of the daemon UI server for a given bind host and port —
/// what a child process on this machine should call.
pub fn local_base_url(bind_host: &str, port: u16) -> String {
    let h = bind_host.trim();
    let host = match h {
        "" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_string(),
        h if h.contains(':') && !h.starts_with('[') => format!("[{h}]"),
        h => h.to_string(),
    };
    format!("http://{host}:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_binds_are_reached_over_loopback() {
        assert_eq!(local_base_url("0.0.0.0", 18788), "http://127.0.0.1:18788");
        assert_eq!(local_base_url("127.0.0.1", 1), "http://127.0.0.1:1");
        assert_eq!(local_base_url("::1", 2), "http://[::1]:2");
        assert_eq!(local_base_url("192.168.1.5", 3), "http://192.168.1.5:3");
    }

    #[tokio::test]
    async fn read_tail_keeps_only_the_end() {
        let data = b"0123456789".repeat(1000);
        let t = read_tail(&data[..], 15).await;
        assert_eq!(t, "567890123456789");
    }

    #[tokio::test]
    async fn a_missing_binary_has_no_version() {
        assert!(binary_version("senclaw-definitely-not-a-binary").await.is_none());
    }
}

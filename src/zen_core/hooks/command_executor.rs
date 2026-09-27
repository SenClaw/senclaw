//! Command hook executor — runs a shell command with JSON input on stdin.
//!
//! Port of TS `hooks/CommandExecutor.ts`.

#[cfg(unix)]
use libc;
use std::collections::HashMap;

use anyhow::{anyhow, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::types::{HookDefinition, HookOutput};

/// Execute a command hook.
///
/// The input JSON is written to the process's stdin. The process is expected to:
/// - Exit 0 and write nothing (or JSON) to stdout → approved (stdout parsed as `HookOutput`)
/// - Exit non-zero → `blocked: true` with stderr as reason
///
/// Timeout defaults to 10 s.
pub async fn execute_command_hook(
    hook: &HookDefinition,
    input_json: &str,
    env: &HashMap<String, String>,
    cancel: Option<&CancellationToken>,
    messages: Option<&[crate::zen_core::Message]>,
) -> Result<HookOutput> {
    let command = match &hook.command {
        Some(c) if !c.trim().is_empty() => c.clone(),
        _ => return Ok(HookOutput::default()),
    };

    if let Some(tok) = cancel {
        if tok.is_cancelled() {
            return Err(anyhow!("Hook aborted"));
        }
    }

    let timeout_secs = hook.timeout.unwrap_or(10);
    let timeout_dur = Duration::from_secs(timeout_secs);

    // Include history in the input JSON if requested
    let input_with_history = if hook.include_history.unwrap_or(false) {
        let history_limit = hook.history_limit.unwrap_or(10);
        if let Some(msgs) = messages {
            let recent_messages = msgs.iter().rev().take(history_limit).collect::<Vec<_>>();
            let mut input_value: serde_json::Value =
                serde_json::from_str(input_json).unwrap_or(serde_json::json!({}));
            let history_array: Vec<serde_json::Value> = recent_messages
                .into_iter()
                .rev()
                .filter_map(|msg| serde_json::to_value(msg).ok())
                .collect();
            input_value["history"] = serde_json::Value::Array(history_array);
            serde_json::to_string(&input_value)?
        } else {
            input_json.to_string()
        }
    } else {
        input_json.to_string()
    };

    #[cfg(windows)]
    let mut child = Command::new("cmd")
        .args(["/c", &command])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .envs(env)
        .spawn()?;

    #[cfg(not(windows))]
    let mut child = Command::new("sh")
        .args(["-c", &command])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .envs(env)
        .spawn()?;

    // Write input to stdin
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        stdin.write_all(input_with_history.as_bytes()).await?;
        // Drop closes stdin
    }

    let result = if let Some(cancel_tok) = cancel {
        let _id = child.id();
        tokio::select! {
            r = timeout(timeout_dur, child.wait_with_output()) => {
                r.map_err(|_| anyhow!("Hook timed out after {timeout_secs}s"))?
            }
            _ = cancel_tok.cancelled() => {
                #[cfg(unix)]
                if let Some(pid) = _id {
                    unsafe { libc::kill(pid as i32, libc::SIGTERM); }
                }
                return Err(anyhow!("Hook aborted"));
            }
        }
    } else {
        timeout(timeout_dur, child.wait_with_output())
            .await
            .map_err(|_| anyhow!("Hook timed out after {timeout_secs}s"))?
    }?;

    let exit_code = result.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&result.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&result.stderr).into_owned();

    if exit_code != 0 {
        let is_blocking = hook.is_blocking();
        return Ok(HookOutput {
            blocked: Some(is_blocking),
            reason: Some(if stderr.trim().is_empty() {
                format!("Exit code {exit_code}")
            } else {
                stderr.trim().to_string()
            }),
            ..Default::default()
        });
    }

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(HookOutput::default());
    }

    match serde_json::from_str::<HookOutput>(trimmed) {
        Ok(output) => Ok(output),
        Err(e) => {
            warn!(
                "[hooks] Command stdout was not valid JSON ({e}): {}",
                &trimmed[..trimmed.len().min(100)]
            );
            Ok(HookOutput {
                response: Some(trimmed.to_string()),
                ..Default::default()
            })
        }
    }
}

//! Bash tool — shell command execution with safety guards.
//!
//! Port of TS `node_modules/sema-core/dist/tools/Bash/`.

use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use tracing::warn;

use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolPermissionInfo, ToolResultMessage};

const MAX_OUTPUT_LENGTH: usize = 30000;
const MAX_TIMEOUT_MS: u64 = 180_000;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Commands banned in all circumstances.
const BANNED_COMMANDS: &[&str] = &[
    "alias",
    "curl",
    "curlie",
    "wget",
    "axel",
    "aria2c",
    "nc",
    "telnet",
    "lynx",
    "w3m",
    "links",
    "httpie",
    "xh",
    "http-prompt",
    "chrome",
    "firefox",
    "safari",
];

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command in a persistent shell session with optional timeout"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command to execute"
                },
                "timeout": {
                    "type": "number",
                    "description": "Optional timeout in milliseconds (max 180000ms)"
                },
                "description": {
                    "type": "string",
                    "description": "Clear, concise description of what this command does in 5-10 words"
                }
            },
            "required": ["command", "description"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if command.trim().is_empty() {
            return Err("Command is required".to_string());
        }

        // Check banned commands
        let first_word = command.trim().split(' ').next().unwrap_or("");
        let base = first_word.split('/').last().unwrap_or(first_word);
        if BANNED_COMMANDS.contains(&base.to_lowercase().as_str()) {
            return Err(format!(
                "Command '{base}' is not allowed for security reasons"
            ));
        }

        // Safe cd check — only allow cd to child directories of original cwd
        check_cd_safety(command, ctx.working_dir)?;

        Ok(())
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let timeout_ms = input
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        // Check abort token
        if ctx.abort.is_cancelled() {
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"interrupted": true}),
                result_for_assistant: "Command was cancelled before execution.".into(),
            }]);
        }

        // Plugins → Sandbox → "exec": run the command inside the OS sandbox,
        // write-jailed to the working directory. When the switch is on but the
        // engine cannot run, the command FAILS rather than silently dropping
        // back to a raw shell — the user asked for the jail.
        if crate::sandbox::policy::current().exec_shell {
            let run =
                crate::sandbox::policy::agent_shell(&command, ctx.working_dir, timeout_ms).await;
            return Ok(vec![match run {
                Ok(run) => {
                    let mut output_text = String::new();
                    if !run.stdout.is_empty() {
                        output_text.push_str(&run.stdout);
                    }
                    if !run.stderr.is_empty() {
                        if !output_text.is_empty() {
                            output_text.push('\n');
                        }
                        output_text.push_str(&run.stderr);
                    }
                    if run.timed_out {
                        if !output_text.is_empty() {
                            output_text.push('\n');
                        }
                        output_text.push_str(&format!(
                            "Command timed out after {timeout_ms}ms (killed)"
                        ));
                    }
                    ToolOutput::Result {
                        data: serde_json::json!({
                            "stdout": run.stdout,
                            "stderr": run.stderr,
                            "exitCode": run.exit_code.unwrap_or(-1),
                            "truncated": run.truncated,
                            "timeout": run.timed_out,
                            "title": get_title(&command),
                            "sandbox": { "isolation": run.isolation, "network": run.network },
                        }),
                        result_for_assistant: output_text,
                    }
                }
                Err(e) => ToolOutput::Result {
                    data: serde_json::json!({"error": true}),
                    result_for_assistant: format!(
                        "Sandboxed execution failed: {e}. \
                         (Bash runs inside the OS sandbox because `exec` enforcement is on in Plugins → Sandbox.)"
                    ),
                },
            }]);
        }

        let cmd = command.clone();
        let working_dir = ctx.working_dir.to_string();

        let result = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            tokio::task::spawn_blocking(move || execute_command(&cmd, &working_dir)),
        )
        .await;

        match result {
            Ok(Ok(Ok((stdout, stderr, exit_code)))) => {
                let mut output_text = String::new();
                if !stdout.is_empty() {
                    output_text.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !output_text.is_empty() {
                        output_text.push('\n');
                    }
                    output_text.push_str(&stderr);
                }

                let truncated = if output_text.len() > MAX_OUTPUT_LENGTH {
                    let cutoff = output_text
                        .char_indices()
                        .nth(MAX_OUTPUT_LENGTH)
                        .map(|(i, _)| i)
                        .unwrap_or(output_text.len());
                    output_text.truncate(cutoff);
                    output_text.push_str("\n... [output truncated]");
                    true
                } else {
                    false
                };

                let title = get_title(&command);
                Ok(vec![ToolOutput::Result {
                    data: serde_json::json!({
                        "stdout": stdout,
                        "stderr": stderr,
                        "exitCode": exit_code,
                        "truncated": truncated,
                        "title": title,
                    }),
                    result_for_assistant: output_text,
                }])
            }
            Ok(Ok(Err(e))) => Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"error": true}),
                result_for_assistant: format!("Command execution failed: {e}"),
            }]),
            Ok(Err(e)) => Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"error": true}),
                result_for_assistant: format!("Internal task error: {e}"),
            }]),
            Err(_elapsed) => Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"timeout": true}),
                result_for_assistant: format!("Command timed out after {timeout_ms}ms"),
            }]),
        }
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let title = data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Bash")
            .to_string();

        let summary = if data
            .get("timeout")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            "Timed out".into()
        } else if data.get("exitCode").and_then(|v| v.as_i64()).unwrap_or(-1) == 0 {
            "Completed".into()
        } else {
            "Completed with errors".into()
        };

        ToolResultMessage {
            title,
            summary,
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let cmd = input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("Bash");
        get_title(cmd)
    }

    fn gen_tool_permission(&self, input: &Value) -> Option<ToolPermissionInfo> {
        let title = self.get_display_title(input);
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");

        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        Some(ToolPermissionInfo {
            title: if title.len() > 300 {
                crate::util::text::truncate_on_char_boundary(&title, 300).to_string()
            } else {
                title
            },
            content: serde_json::json!({
                "command": cmd,
                "description": description,
            }),
        })
    }
}

fn get_title(command: &str) -> String {
    // Process heredoc markers for display
    let cleaned = command.replace("<<'EOF'", "").replace("<<EOF", "");
    if cleaned.len() > 100 {
        format!(
            "{}...",
            crate::util::text::truncate_on_char_boundary(&cleaned, 100)
        )
    } else {
        cleaned
    }
}

fn execute_command(command: &str, working_dir: &str) -> Result<(String, String, i32)> {
    let output = Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(working_dir)
        .output()
        .context("Failed to execute command")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    Ok((stdout, stderr, exit_code))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_banned_commands() {
        for cmd in BANNED_COMMANDS {
            assert!(BANNED_COMMANDS.contains(cmd));
        }
    }

    #[test]
    fn test_validate_rejects_empty_command() {
        let tool = BashTool;
        let ctx = ToolContext {
            agent_id: "test",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tool.validate_input(
            &serde_json::json!({"command": "", "description": "empty"}),
            &ctx,
        ));
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_rejects_banned_curl() {
        let tool = BashTool;
        let ctx = ToolContext {
            agent_id: "test",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tool.validate_input(
            &serde_json::json!({"command": "curl http://evil.com", "description": "bad"}),
            &ctx,
        ));
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_allows_safe_command() {
        let tool = BashTool;
        let ctx = ToolContext {
            agent_id: "test",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tool.validate_input(
            &serde_json::json!({"command": "ls -la", "description": "list files"}),
            &ctx,
        ));
        assert!(result.is_ok());
    }

    #[test]
    fn test_get_title_truncates_long_command() {
        let long_cmd = "a".repeat(200);
        let title = get_title(&long_cmd);
        assert!(title.len() <= 103); // 100 + "..."
    }

    #[test]
    fn test_execute_command_ls() {
        let result = execute_command("echo hello", "/tmp");
        assert!(result.is_ok());
        let (stdout, _, code) = result.unwrap();
        assert_eq!(code, 0);
        assert!(stdout.contains("hello"));
    }

    /// An empty working_dir makes `Command::current_dir("")` fail to spawn with
    /// ENOENT — the failure mode that returned `{"error": true}` for every Bash
    /// call. The engine must never be handed an empty working dir; this test
    /// documents why (see `default_working_dir` in agent_pool/engine.rs).
    #[test]
    fn test_execute_command_empty_working_dir_fails_to_spawn() {
        let result = execute_command("echo hello", "");
        assert!(
            result.is_err(),
            "empty working_dir must fail to spawn (ENOENT), not silently run"
        );
    }
}

fn check_cd_safety(command: &str, working_dir: &str) -> std::result::Result<(), String> {
    // Parse multi-line commands; check each segment that contains 'cd'
    for part in command.split("&&").chain(command.split(';')) {
        let trimmed = part.trim();
        let parts: Vec<&str> = trimmed.split(' ').collect();
        if parts.first() == Some(&"cd") && parts.len() >= 2 {
            let target = parts[1].trim_matches(|c| c == '\'' || c == '"');
            let target_path = if target.starts_with('/') {
                std::path::PathBuf::from(target)
            } else {
                std::path::PathBuf::from(working_dir).join(target)
            };

            // Only allow cd to subdirectories of working_dir
            let canonical_working = std::path::PathBuf::from(working_dir)
                .canonicalize()
                .unwrap_or_else(|_| working_dir.into());

            if !target_path.starts_with(&canonical_working) {
                let msg = format!(
                    "cd to '{}' was blocked. Agent may only change to child directories of {}",
                    target_path.display(),
                    canonical_working.display()
                );
                warn!("{msg}");
                return Err(msg);
            }
        }
    }
    Ok(())
}

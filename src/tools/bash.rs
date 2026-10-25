//! Bash tool — shell command execution with safety guards.
//!
//! Port of TS `node_modules/sema-core/dist/tools/Bash/`.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tracing::warn;

use crate::bg_tasks::{self, AdoptSpec, Held, StartSpec};
use crate::tools::bg_jobs::{BgJobManager, JobStatus};
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
        "Execute a bash command with an optional timeout. A server, a watcher or anything that keeps running \
         (`npm run dev`, `python -m http.server`) must be started with run_in_background=true: it returns \
         at once with a job id, keeps running after the turn, and shows in the person's Background tasks \
         list where they can stop it. Do not start it with `nohup … &` or a trailing `&` in a foreground call."
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
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Start the command and return at once with a job id; for dev servers, watchers and other long-running processes. Read its output with PeekBgJob, end it with StopBgJob."
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
        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let background = input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

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

        if background {
            return Ok(vec![
                start_in_background(&command, ctx.working_dir, &description).await,
            ]);
        }

        // Plugins → Sandbox → "exec": run the command inside the OS sandbox,
        // write-jailed to the working directory. When the switch is on but the
        // engine cannot run, the command FAILS rather than silently dropping
        // back to a raw shell — the user asked for the jail.
        if crate::sandbox::policy::current().shell_sandboxed() {
            let run = crate::sandbox::policy::agent_shell_foreground(
                &command,
                ctx.working_dir,
                timeout_ms,
            )
            .await;
            return Ok(vec![match run {
                Ok((run, leftover)) => {
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
                    let left = adopt_leftovers(
                        leftover.group,
                        leftover.held,
                        &command,
                        ctx.working_dir,
                        &description,
                        &run.isolation,
                    )
                    .await;
                    output_text.push_str(&left.note);
                    // The exit marker comes after the background note: it is the last line the
                    // agent loop reads (`finish_check::result_failed`).
                    let code = run.exit_code.unwrap_or(-1);
                    let result_for_assistant = if run.timed_out {
                        output_text
                    } else if code != 0 {
                        format!("{output_text}\n[exit code {code}]")
                    } else if output_text.trim().is_empty() {
                        "(no output; exit code 0)".to_string()
                    } else {
                        output_text
                    };
                    ToolOutput::Result {
                        data: serde_json::json!({
                            "stdout": run.stdout,
                            "stderr": run.stderr,
                            "exitCode": code,
                            "truncated": run.truncated,
                            "timeout": run.timed_out,
                            "title": get_title(&command),
                            "sandbox": { "isolation": run.isolation, "network": run.network },
                            "background": left.jobs,
                        }),
                        result_for_assistant,
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

        match bg_tasks::shell::run_raw(&command, ctx.working_dir, timeout_ms).await {
            Ok(run) if run.timed_out => Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"timeout": true}),
                result_for_assistant: format!("Command timed out after {timeout_ms}ms"),
            }]),
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

                let left = adopt_leftovers(
                    run.group,
                    run.held,
                    &command,
                    ctx.working_dir,
                    &description,
                    "",
                )
                .await;
                output_text.push_str(&left.note);
                // Say how the command ended. Without it a model reads a failing
                // test run's tail as success, and an empty result as "nothing
                // happened" - the two commonest ways a small model misreports a
                // build. The marker is also what the agent loop reads to know
                // that the last check failed, so it stays the last line, after
                // the note about anything left running.
                let exit_code = run.exit_code;
                let result_for_assistant = if exit_code != 0 {
                    format!("{output_text}\n[exit code {exit_code}]")
                } else if output_text.trim().is_empty() {
                    "(no output; exit code 0)".to_string()
                } else {
                    output_text
                };
                Ok(vec![ToolOutput::Result {
                    data: serde_json::json!({
                        "stdout": run.stdout,
                        "stderr": run.stderr,
                        "exitCode": run.exit_code,
                        "truncated": truncated,
                        "title": get_title(&command),
                        "background": left.jobs,
                    }),
                    result_for_assistant,
                }])
            }
            Err(e) => Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"error": true}),
                result_for_assistant: e,
            }]),
        }
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let title = data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Bash")
            .to_string();

        let summary = if data.get("background").and_then(|v| v.as_bool()) == Some(true) {
            format!(
                "Running in the background ({})",
                data.get("jobId").and_then(|v| v.as_str()).unwrap_or("")
            )
        } else if data
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


/// How long a background start is watched before returning: a command that dies
/// at once (port taken, missing script) is a failed start, not a task.
const EARLY_LOOK: Duration = Duration::from_millis(2000);
const EARLY_OUTPUT_MAX: usize = 4000;

/// `run_in_background`: start the command, look at its first moments, return its id.
async fn start_in_background(command: &str, working_dir: &str, description: &str) -> ToolOutput {
    let spec = StartSpec {
        command: command.to_string(),
        cwd: working_dir.to_string(),
        description: description.to_string(),
        chat_jid: bg_tasks::current_chat(),
    };
    let job = match bg_tasks::start_background(spec).await {
        Ok(j) => j,
        Err(e) => {
            return ToolOutput::Result {
                data: serde_json::json!({"error": true}),
                result_for_assistant: e,
            }
        }
    };
    let status = BgJobManager::global()
        .wait(&job.id, EARLY_LOOK)
        .await
        .unwrap_or(JobStatus::Running);
    let output = bg_tasks::tail(&job.output_snapshot(), EARLY_OUTPUT_MAX);
    let title = get_title(command);

    if status != JobStatus::Running {
        let code = job.state().exit_code;
        let mut text = format!(
            "The command exited within {}s (exit code {}) — nothing was left running.",
            EARLY_LOOK.as_secs(),
            code.map_or("unknown".to_string(), |c| c.to_string())
        );
        if !output.is_empty() {
            text.push_str("\nOutput:\n");
            text.push_str(&output);
        }
        return ToolOutput::Result {
            data: serde_json::json!({
                "stdout": output,
                "stderr": "",
                "exitCode": code.unwrap_or(-1),
                "title": title,
                "jobId": job.id,
            }),
            result_for_assistant: text,
        };
    }

    bg_tasks::refresh_now(&job).await;
    let st = job.state();
    let ports = if st.ports.is_empty() {
        String::new()
    } else {
        format!(
            ", listening on {}",
            st.ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(" ")
        )
    };
    let mut text = format!(
        "Started in the background: {} (pid {}{ports}). It keeps running after this turn; the person can see it in \
         their Background tasks list and stop it there. Read its output with PeekBgJob {{job_id: \"{}\", wait: false}}, \
         stop it with StopBgJob {{job_id: \"{}\"}}.",
        job.id,
        st.pid.unwrap_or(0),
        job.id,
        job.id,
    );
    if !output.is_empty() {
        text.push_str("\nOutput so far:\n");
        text.push_str(&output);
    }
    ToolOutput::Result {
        data: serde_json::json!({
            "stdout": output,
            "stderr": "",
            "exitCode": 0,
            "title": title,
            "jobId": job.id,
            "pid": st.pid,
            "ports": st.ports,
            "background": true,
        }),
        result_for_assistant: text,
    }
}

/// What a foreground command left running, registered as background tasks.
struct Left {
    /// Appended to the text the model reads; empty when nothing was left.
    note: String,
    jobs: Vec<Value>,
}

async fn adopt_leftovers(
    group: Option<u32>,
    held: Option<Held>,
    command: &str,
    cwd: &str,
    description: &str,
    isolation: &str,
) -> Left {
    let none = Left { note: String::new(), jobs: Vec::new() };
    let Some(group) = group else { return none };
    let chat = bg_tasks::current_chat();
    let spec = AdoptSpec { command, cwd, description, chat_jid: &chat, isolation };
    let Some(job) = bg_tasks::adopt(group, held, spec).await else {
        return none;
    };
    bg_tasks::refresh_now(&job).await;
    let st = job.state();
    Left {
        note: bg_tasks::adopted_note(&job),
        jobs: vec![serde_json::json!({"jobId": job.id, "pid": st.pid, "ports": st.ports})],
    }
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

    #[tokio::test]
    async fn test_execute_command_ls() {
        let run = bg_tasks::shell::run_raw("echo hello", "/tmp", 10_000).await.unwrap();
        assert_eq!(run.exit_code, 0);
        assert!(run.stdout.contains("hello"));
    }

    #[cfg(unix)]
    fn ctx() -> ToolContext<'static> {
        ToolContext {
            agent_id: "test",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        }
    }

    #[cfg(unix)]
    fn result_of(out: Vec<ToolOutput>) -> (Value, String) {
        match out.into_iter().next() {
            Some(ToolOutput::Result { data, result_for_assistant }) => (data, result_for_assistant),
            _ => panic!("Bash must answer with one result"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_in_background_answers_with_a_job_id_and_the_job_stops() {
        let c = ctx();
        let call = BashTool.call(
            serde_json::json!({"command": "echo up; sleep 60", "description": "sleeper", "run_in_background": true}),
            &c,
        );
        let (data, text) = result_of(bg_tasks::with_chat("web:bash-unit-1", call).await.unwrap());
        assert!(text.contains("Started in the background: bash-"), "{text}");
        assert!(text.contains("up"), "the first output is in the answer: {text}");
        let id = data["jobId"].as_str().unwrap().to_string();

        let job = BgJobManager::global().get(&id).expect("registered");
        assert_eq!(job.status(), JobStatus::Running);
        assert_eq!(job.meta.chat_jid, "web:bash-unit-1", "the job belongs to the calling chat");
        assert_eq!(job.meta.description, "sleeper");
        assert_eq!(
            BashTool.gen_tool_result_message(&data, &Value::Null).summary,
            format!("Running in the background ({id})")
        );
        assert!(job.kill().await);
        assert_eq!(job.status(), JobStatus::Stopped);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_background_command_that_fails_at_once_is_reported_as_failed_not_as_a_task() {
        let c = ctx();
        let call = BashTool.call(
            serde_json::json!({"command": "echo boom; exit 3", "description": "fail", "run_in_background": true}),
            &c,
        );
        let (data, text) = result_of(bg_tasks::with_chat("web:bash-unit-2", call).await.unwrap());
        assert!(text.contains("exited within") && text.contains("exit code 3") && text.contains("boom"), "{text}");
        assert_eq!(data["exitCode"], 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_foreground_command_that_leaves_a_process_names_its_job_to_the_model() {
        let c = ctx();
        let call = BashTool.call(
            serde_json::json!({
                "command": "nohup sleep 60 > /dev/null 2>&1 &\necho started",
                "description": "leave a sleeper"
            }),
            &c,
        );
        let (data, text) = result_of(bg_tasks::with_chat("web:bash-unit-3", call).await.unwrap());
        assert!(text.starts_with("started"), "{text}");
        assert!(text.contains("Background process left running by this command: bash-"), "{text}");
        let id = data["background"][0]["jobId"].as_str().expect("the result carries the job id").to_string();
        let job = BgJobManager::global().get(&id).unwrap();
        assert_eq!(job.meta.chat_jid, "web:bash-unit-3");
        assert_eq!(job.meta.description, "leave a sleeper");
        assert!(job.kill().await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_clean_foreground_command_adds_no_note_and_no_job() {
        let c = ctx();
        let c = ctx();
        let call = BashTool.call(serde_json::json!({"command": "echo plain", "description": "plain"}), &c);
        let (data, text) = result_of(bg_tasks::with_chat("web:bash-unit-4", call).await.unwrap());
        assert_eq!(text.trim(), "plain");
        assert_eq!(data["background"], serde_json::json!([]));
        assert!(bg_tasks::views(Some("web:bash-unit-4"), true).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_foreground_timeout_ends_the_whole_command() {
        let started = std::time::Instant::now();
        let c = ctx();
        let call = BashTool.call(
            serde_json::json!({"command": "sleep 60 & sleep 60", "description": "hang", "timeout": 1000}),
            &c,
        );
        let (data, text) = result_of(call.await.unwrap());
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(data["timeout"], true);
        assert!(text.contains("timed out"), "{text}");
    }

    /// An empty working_dir makes `Command::current_dir("")` fail to spawn with
    /// ENOENT — the failure mode that returned `{"error": true}` for every Bash
    /// call. The engine must never be handed an empty working dir; this test
    /// documents why (see `default_working_dir` in agent_pool/engine.rs).
    #[tokio::test]
    async fn test_execute_command_empty_working_dir_fails_to_spawn() {
        let result = bg_tasks::shell::run_raw("echo hello", "", 10_000).await;
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

//! Step runners — execute a single step by kind.
//!
//! Port of `SemaClaw/src/workflow/stepRunners.ts`.
//!
//! Both kinds return a uniform [`StepRunResult`] so the DAG scheduler is
//! agnostic of agent vs script.
//!   - agent: built on `isolated_runner::run_one_shot` (isolated session,
//!     never touches live agents). Three-channel mapping:
//!       persona.system_prompt → system_prompt ; guidance → custom_rules ;
//!       prompt → process_user_input
//!   - script: child process, cwd = run_dir, env = process env + WF_* vars,
//!     stdout captured as `result`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::agent::isolated_runner::{
    run_one_shot, McpInject, OnActivity, OneShotOptions, SkipPermissions,
};
use crate::agent::persona_registry::PersonaConfig;
use crate::zen_core::{AgentMode, McpServerConfig};

use super::template::{build_script_env, render};
use super::types::{sanitize_name, RenderContext, WorkflowDef, WorkflowStep};

/// Default step timeout (seconds) for both kinds.
pub const DEFAULT_STEP_TIMEOUT_SECS: u64 = 600;

/// Script results longer than this (chars) spill to disk so downstream env
/// (ARG_MAX) and the run record stay small.
const RESULT_SPILL_THRESHOLD: usize = 5000;
/// Preview chars kept in `result` after spilling.
const RESULT_PREVIEW_CHARS: usize = 300;

/// Script stdout cap (memory safety).
const SCRIPT_MAX_BUFFER: usize = 16 * 1024 * 1024;

/// Default agent toolset (mirrors VirtualWorkerPool's pooled tools).
const DEFAULT_AGENT_TOOLS: &[&str] = &[
    "Bash",
    "Glob",
    "Grep",
    "Read",
    "Write",
    "Edit",
    "TodoWrite",
    "Skill",
    "NotebookEdit",
];
/// Orchestration tools excluded from workflow agent steps (steps don't
/// orchestrate or start nested workflows).
const EXCLUDED_TOOLS: &[&str] = &["Task", "AskUserQuestion"];

/// Agent-loop turn budget for workflow agent steps. Browser-driven research
/// burns ~2 turns per page, so the engine default of 30 kills legitimate
/// multi-source runs (the persona then flails until the provider forces a
/// useless "I give up" answer). Override with `SENCLAW_WORKFLOW_MAX_TURNS`;
/// mirrors VirtualWorkerPool's `SENCLAW_VIRTUAL_MAX_TURNS` (default 60).
fn workflow_agent_max_turns() -> usize {
    std::env::var("SENCLAW_WORKFLOW_MAX_TURNS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(60)
}

#[derive(Debug, Clone, Default)]
pub struct StepRunResult {
    pub result: String,
    pub failed: bool,
    pub error: Option<String>,
    /// Ended by cancellation (abort): the executor records the step as
    /// `skipped` rather than `failed`.
    pub aborted: bool,
    /// Worth retrying (session error / produced no text) — transient LLM
    /// failures, not deterministic ones like timeouts or cancellation.
    pub retryable: bool,
    /// Agent steps: the rendered guidance (→ run record guidance_snapshot).
    pub guidance_snapshot: Option<String>,
}

/// Execute an agent step: isolated session via `run_one_shot`.
/// `extra_mcp_servers` (e.g. browser-mcp) are injected into the session so
/// personas like web-scout / browser-agent actually have their tools —
/// without them the model calls missing tools until it gives up.
pub async fn run_agent_step(
    step: &WorkflowStep,
    persona: &PersonaConfig,
    def: &WorkflowDef,
    ctx: &RenderContext,
    skills_extra_dirs: Vec<String>,
    extra_mcp_servers: &[McpServerConfig],
    on_activity: Option<OnActivity>,
    cancel: CancellationToken,
) -> StepRunResult {
    let prompt = render(step.prompt.as_deref(), ctx);
    let merged_guidance = [def.guidance.as_deref(), step.guidance.as_deref()]
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>()
        .join("\n\n");
    let guidance = render(Some(&merged_guidance), ctx);

    let use_tools: Vec<String> = persona
        .tools
        .clone()
        .unwrap_or_else(|| DEFAULT_AGENT_TOOLS.iter().map(|t| t.to_string()).collect())
        .into_iter()
        .filter(|t| !EXCLUDED_TOOLS.contains(&t.as_str()))
        .collect();

    let timeout_secs = step.timeout.unwrap_or(DEFAULT_STEP_TIMEOUT_SECS);
    let opts = OneShotOptions {
        prompt,
        working_dir: ctx.run_dir.clone(),
        agent_data_dir: None,
        instance_id: Some(format!(
            "wf-{}-{:x}",
            sanitize_name(&step.id),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        )),
        use_tools,
        skills_extra_dirs,
        system_prompt: (!persona.system_prompt.is_empty()).then(|| persona.system_prompt.clone()),
        custom_rules: (!guidance.is_empty()).then(|| guidance.clone()),
        agent_mode: AgentMode::Agent,
        mcp_configs: extra_mcp_servers
            .iter()
            .map(|config| McpInject {
                config: config.clone(),
                scope: "virtual".to_string(),
            })
            .collect(),
        timeout: Some(Duration::from_secs(timeout_secs)),
        // Unattended: skip all permission prompts.
        skip_permissions: SkipPermissions::default(),
        cancel: Some(cancel),
        max_agent_turns: Some(workflow_agent_max_turns()),
        model_config_id: None,
        on_activity,
            // Workflow steps are not eval runs; nothing to record.
        trajectory_jid: None,
    };

    let guidance_snapshot = (!guidance.is_empty()).then_some(guidance);
    match run_one_shot(opts).await {
        Ok(res) => {
            // An agent step exists to produce `result` — finishing "cleanly"
            // with no text (LLM error swallowed, model answered nothing) must
            // fail loud instead of silently feeding "" to downstream steps.
            let empty = res.text.trim().is_empty();
            let error = if res.aborted {
                Some("cancelled".to_string())
            } else if res.errored {
                Some(format!(
                    "agent session error: {}",
                    res.error_message.as_deref().unwrap_or("unknown")
                ))
            } else if res.timed_out {
                Some(format!("agent step timed out after {timeout_secs}s"))
            } else if empty {
                Some(
                    "agent finished without producing any text (check model/daemon logs)"
                        .to_string(),
                )
            } else {
                None
            };
            // Session errors / empty replies are usually transient LLM
            // hiccups → retryable. Timeouts and cancellation are not.
            let retryable = !res.aborted && !res.timed_out && (res.errored || empty);
            StepRunResult {
                result: res.text,
                failed: error.is_some(),
                aborted: res.aborted,
                retryable,
                error,
                guidance_snapshot,
            }
        }
        Err(e) => StepRunResult {
            result: String::new(),
            failed: true,
            aborted: false,
            retryable: true,
            error: Some(format!("agent session error: {e:#}")),
            guidance_snapshot,
        },
    }
}

/// Execute a script step: child process, cwd = run_dir, WF_* env injected.
pub async fn run_script_step(
    step: &WorkflowStep,
    def: &WorkflowDef,
    ctx: &RenderContext,
    observe_dir: &str,
    workflow_dir: Option<&str>,
    shell_override: Option<&str>,
    cancel: CancellationToken,
) -> StepRunResult {
    // Command: inline `run` wins; otherwise `script_file` resolved against
    // the definition dir and quoted (must be executable / have a shebang).
    let command = match (&step.run, &step.script_file) {
        (Some(run), _) => run.clone(),
        (None, Some(file)) => {
            let resolved = if Path::new(file).is_absolute() {
                PathBuf::from(file)
            } else {
                def.file_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(file)
            };
            format!("{:?}", resolved.to_string_lossy())
        }
        (None, None) => {
            return StepRunResult {
                failed: true,
                error: Some(format!(
                    "script step \"{}\" has neither run nor scriptFile",
                    step.id
                )),
                ..Default::default()
            }
        }
    };

    let env = build_script_env(ctx, observe_dir, workflow_dir);
    let timeout = Duration::from_secs(step.timeout.unwrap_or(DEFAULT_STEP_TIMEOUT_SECS));

    match run_shell(
        &command,
        &ctx.run_dir,
        &env,
        timeout,
        shell_override,
        cancel.clone(),
    )
    .await
    {
        Ok(stdout) => StepRunResult {
            result: spill_large_result(stdout.trim(), &ctx.run_dir, &step.id),
            ..Default::default()
        },
        Err(e) => {
            let aborted = cancel.is_cancelled();
            StepRunResult {
                result: spill_large_result(e.stdout.trim(), &ctx.run_dir, &step.id),
                failed: true,
                aborted,
                // Scripts are deterministic — retrying wouldn't change the outcome.
                retryable: false,
                error: Some(if aborted {
                    "cancelled".to_string()
                } else {
                    e.message
                }),
                guidance_snapshot: None,
            }
        }
    }
}

struct ShellError {
    message: String,
    stdout: String,
}

/// Resolve the shell used for script steps:
///   - `shell_override` (config `SENCLAW_WORKFLOW_SHELL`) if it exists
///   - POSIX: `/bin/sh`
///   - Windows: `cmd /C` fallback (POSIX syntax will fail — document in skill)
fn resolve_shell(shell_override: Option<&str>) -> (String, Vec<String>) {
    if let Some(sh) = shell_override {
        if Path::new(sh).exists() {
            return (sh.to_string(), vec!["-c".to_string()]);
        }
    }
    if cfg!(windows) {
        ("cmd".to_string(), vec!["/C".to_string()])
    } else {
        ("/bin/sh".to_string(), vec!["-c".to_string()])
    }
}

/// Run one shell command capturing stdout.
///
/// POSIX: the child becomes the leader of its own process group
/// (`process_group(0)`) so cancel/timeout/over-buffer can SIGKILL the whole
/// group — grandchildren holding the stdout pipe die too, never leaving
/// orphans still writing into the workspace.
async fn run_shell(
    command: &str,
    cwd: &str,
    extra_env: &HashMap<String, String>,
    timeout: Duration,
    shell_override: Option<&str>,
    cancel: CancellationToken,
) -> Result<String, ShellError> {
    let (shell, args) = resolve_shell(shell_override);

    let mut cmd = tokio::process::Command::new(&shell);
    cmd.args(&args)
        .arg(command)
        .current_dir(cwd)
        .envs(extra_env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().map_err(|e| ShellError {
        message: format!("spawn failed: {e}"),
        stdout: String::new(),
    })?;

    #[cfg(unix)]
    let pgid = child.id().map(|pid| pid as i32);

    let kill_group = move || {
        #[cfg(unix)]
        if let Some(pgid) = pgid {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    };

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");

    // Reader tasks with a hard cap: exceeding the cap kills the group.
    let stdout_task = {
        let kill = kill_group.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let mut over = false;
            loop {
                match stdout_pipe.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if buf.len() + n > SCRIPT_MAX_BUFFER {
                            buf.extend_from_slice(&chunk[..SCRIPT_MAX_BUFFER - buf.len()]);
                            over = true;
                            kill();
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
            }
            (String::from_utf8_lossy(&buf).into_owned(), over)
        })
    };
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stderr_pipe.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() < SCRIPT_MAX_BUFFER {
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let mut timed_out = false;
    let mut cancelled = false;

    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            cancelled = true;
            kill_group();
            let _ = child.kill().await;
            child.wait().await.ok()
        }
        _ = tokio::time::sleep(timeout) => {
            timed_out = true;
            kill_group();
            let _ = child.kill().await;
            child.wait().await.ok()
        }
        status = child.wait() => status.ok(),
    };

    let (stdout, over_buffer) = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    if over_buffer {
        return Err(ShellError {
            message: format!("script exceeded maxBuffer ({SCRIPT_MAX_BUFFER} bytes)"),
            stdout,
        });
    }
    if timed_out {
        return Err(ShellError {
            message: "script timed out".to_string(),
            stdout,
        });
    }
    if cancelled {
        return Err(ShellError {
            message: "cancelled".to_string(),
            stdout,
        });
    }
    match status {
        Some(st) if st.success() => Ok(stdout),
        Some(st) => Err(ShellError {
            message: {
                let stderr_trim = stderr.trim();
                if stderr_trim.is_empty() {
                    format!("script exited with code {}", st.code().unwrap_or(-1))
                } else {
                    stderr_trim.to_string()
                }
            },
            stdout,
        }),
        None => Err(ShellError {
            message: "script wait failed".to_string(),
            stdout,
        }),
    }
}

/// Results over the threshold spill in full to `<run_dir>/.results/<step>.txt`;
/// `result` becomes a pointer line (path + total length) plus an N-char
/// preview. Keeps `{{steps.x.result}}` / `WF_STEP_x_RESULT` injections from
/// blowing up downstream env, and keeps the run record small. Downstream
/// consumers needing the full data read the file at the printed path.
fn spill_large_result(result: &str, run_dir: &str, step_id: &str) -> String {
    if result.chars().count() <= RESULT_SPILL_THRESHOLD {
        return result.to_string();
    }
    let preview: String = result.chars().take(RESULT_PREVIEW_CHARS).collect();
    let total = result.chars().count();
    let dir = Path::new(run_dir).join(".results");
    let file = dir.join(format!("{}.txt", sanitize_name(step_id)));
    match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&file, result)) {
        Ok(_) => format!(
            "[truncated: {total} chars total; full output saved to {}]\n{preview}…",
            file.display()
        ),
        // Even if the spill fails, never push a huge string into env/records.
        Err(_) => format!("[truncated: {total} chars total]\n{preview}…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> RenderContext {
        RenderContext {
            inputs: HashMap::from([("who".to_string(), "world".to_string())]),
            step_results: HashMap::new(),
            run_dir: dir.to_string_lossy().to_string(),
        }
    }

    fn script_step(id: &str, run: &str, timeout: Option<u64>) -> WorkflowStep {
        WorkflowStep {
            id: id.to_string(),
            kind: super::super::types::StepKind::Script,
            depends_on: vec![],
            timeout,
            guidance: None,
            observe: None,
            persona: None,
            prompt: None,
            run: Some(run.to_string()),
            script_file: None,
        }
    }

    fn def(dir: &Path) -> WorkflowDef {
        WorkflowDef {
            name: "t".into(),
            description: None,
            version: None,
            inputs: vec![],
            guidance: None,
            workspace: None,
            steps: vec![],
            file_path: dir.join("t.md"),
            source: "user".into(),
        }
    }

    #[tokio::test]
    async fn script_step_captures_stdout_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let step = script_step("s1", "echo \"hi $WF_INPUT_WHO from $WF_RUN_DIR\"", None);
        let c = ctx(dir.path());
        let res = run_script_step(
            &step,
            &def(dir.path()),
            &c,
            "/tmp/obs",
            Some(&c.run_dir),
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(!res.failed, "{:?}", res.error);
        assert_eq!(
            res.result,
            format!("hi world from {}", dir.path().to_string_lossy())
        );
    }

    #[tokio::test]
    async fn script_step_failure_captures_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let step = script_step("s1", "echo oops >&2; exit 3", None);
        let c = ctx(dir.path());
        let res = run_script_step(
            &step,
            &def(dir.path()),
            &c,
            "/tmp/obs",
            None,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(res.failed);
        assert_eq!(res.error.as_deref(), Some("oops"));
    }

    #[tokio::test]
    async fn script_step_timeout_kills_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let step = script_step("s1", "sleep 30 & wait", Some(1));
        let c = ctx(dir.path());
        let started = std::time::Instant::now();
        let res = run_script_step(
            &step,
            &def(dir.path()),
            &c,
            "/tmp/obs",
            None,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(res.failed);
        assert!(res.error.as_deref().unwrap().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn script_step_cancel_marks_aborted() {
        let dir = tempfile::tempdir().unwrap();
        let step = script_step("s1", "sleep 30", None);
        let c = ctx(dir.path());
        let token = CancellationToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            t2.cancel();
        });
        let res = run_script_step(&step, &def(dir.path()), &c, "/tmp/obs", None, None, token).await;
        assert!(res.failed && res.aborted);
        assert_eq!(res.error.as_deref(), Some("cancelled"));
    }

    #[tokio::test]
    async fn large_result_spills_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let step = script_step("big", "head -c 20000 /dev/zero | tr '\\0' 'x'", None);
        let c = ctx(dir.path());
        let res = run_script_step(
            &step,
            &def(dir.path()),
            &c,
            "/tmp/obs",
            None,
            None,
            CancellationToken::new(),
        )
        .await;
        assert!(!res.failed);
        assert!(res.result.starts_with("[truncated: 20000 chars total"));
        let spilled = dir.path().join(".results").join("big.txt");
        assert_eq!(std::fs::read_to_string(spilled).unwrap().len(), 20000);
    }

    #[test]
    fn spill_is_utf8_safe() {
        let s = "é".repeat(6000);
        let dir = tempfile::tempdir().unwrap();
        let out = spill_large_result(&s, &dir.path().to_string_lossy(), "u");
        assert!(out.contains("6000 chars total"));
    }
}

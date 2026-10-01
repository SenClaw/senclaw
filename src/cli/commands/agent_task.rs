//! `senclaw agent-task` — one-shot disposable agent task CLI.
//!
//! Port of `code-old/SenClaw/src/cli/commands/agent-task.ts`.
//!
//! Typical use: a hook script assembles the full prompt (template + history +
//! existing wiki sections), then pipes/passes it here and parses the JSON output.
//!
//! Recursion guard: sets `SENCLAW_INTERNAL_AGENT=1` so any child hook that
//! checks this env var can skip recursive spawns.
//!
//! Exit codes mirror the TS version:
//!   - 0   success
//!   - 2   empty prompt or bad path
//!   - 3   `--output json` but result is not parseable JSON
//!   - 124 timed out

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::{Args, ValueEnum};

use crate::agent::isolated_runner::{run_one_shot, OneShotOptions, SkipPermissions};
use crate::zen_core::AgentMode;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFmt {
    /// Print the final agent text verbatim.
    Text,
    /// Parse the final text as JSON (with code-fence fallback) and re-emit canonical JSON.
    Json,
    /// Join all `message:complete` texts with `\n---\n`.
    Raw,
}

impl Default for OutputFmt {
    fn default() -> Self {
        OutputFmt::Text
    }
}

#[derive(Debug, Args, Clone, Default)]
pub struct AgentTaskCmd {
    /// Inline prompt text.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Path to prompt file. Use "-" for stdin.
    #[arg(long = "prompt-file")]
    pub prompt_file: Option<String>,
    /// Working directory (defaults to CWD).
    #[arg(long = "working-dir")]
    pub working_dir: Option<String>,
    /// Agent data dir (CLAUDE.md, .sen/). Defaults to working-dir.
    #[arg(long = "agent-data-dir")]
    pub agent_data_dir: Option<String>,
    /// Comma-separated tool whitelist. Empty = all tools.
    #[arg(long)]
    pub tools: Option<String>,
    /// Extra skills directories (repeatable).
    #[arg(long = "skills-dir")]
    pub skills_dir: Vec<String>,
    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    pub output: OutputFmt,
    /// Timeout in milliseconds (defaults to engine default — 5 minutes).
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Multi-tenant engine instance id (auto-generated when omitted).
    #[arg(long = "instance-id")]
    pub instance_id: Option<String>,
    /// Override system prompt.
    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,
    /// Record this run as a trajectory under the given id, for the evals
    /// runner (`scripts/evals/run.py`). Turn files land in
    /// `<senclaw home>/trajectories/<id>/`, and the directory is printed on
    /// stderr when the run ends. Recording stays off for everything else and
    /// nothing is written to `enabled.json`.
    #[arg(long = "trajectory")]
    pub trajectory: Option<String>,
}

pub async fn run(cmd: AgentTaskCmd) -> Result<()> {
    std::env::set_var("SENCLAW_INTERNAL_AGENT", "1");

    // An OAuth-backed model (Antigravity, Copilot, …) reads its bearer token
    // from the process-global OAuth manager, and only the daemon used to
    // install it — so every CLI run of such a model died with "OAuth subsystem
    // is not initialised", with no API-key behaviour to fall back to. Found by
    // running the evals harness, which drives this command. `init` is
    // `get_or_init`, so this is a no-op wherever the daemon already ran it.
    let cfg = crate::config::Config::from_env();
    crate::providers::oauth::init(crate::providers::oauth::store::default_path(
        &cfg.paths.global_config_path,
    ));

    let prompt = read_prompt_input(&cmd)?.trim().to_string();
    if prompt.is_empty() {
        eprintln!("Error: prompt is empty (use --prompt, --prompt-file, or pipe to stdin)");
        std::process::exit(2);
    }

    let working_dir = match cmd.working_dir.as_deref() {
        Some(d) => resolve_user_path(d),
        None => std::env::current_dir()?,
    };
    let agent_data_dir = match cmd.agent_data_dir.as_deref() {
        Some(d) => resolve_user_path(d),
        None => working_dir.clone(),
    };

    assert_dir_exists(&working_dir, "--working-dir");
    if cmd.agent_data_dir.is_some() {
        assert_dir_exists(&agent_data_dir, "--agent-data-dir");
    }

    let use_tools: Vec<String> = cmd
        .tools
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let skills_extra_dirs: Vec<String> = cmd
        .skills_dir
        .iter()
        .map(|d| resolve_user_path(d).to_string_lossy().to_string())
        .collect();

    let timeout = cmd.timeout.filter(|t| *t > 0).map(Duration::from_millis);

    let opts = OneShotOptions {
        prompt: prompt.clone(),
        working_dir: working_dir.to_string_lossy().to_string(),
        agent_data_dir: Some(agent_data_dir.to_string_lossy().to_string()),
        instance_id: cmd.instance_id.clone().or_else(|| {
            Some(format!(
                "agent-task-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            ))
        }),
        use_tools,
        skills_extra_dirs,
        system_prompt: cmd.system_prompt.clone(),
        custom_rules: None,
        agent_mode: AgentMode::Agent,
        mcp_configs: Vec::new(),
        timeout,
        skip_permissions: SkipPermissions::default(),
        cancel: None,
        max_agent_turns: None,
        model_config_id: None,
        on_activity: None,
        trajectory_jid: cmd.trajectory.clone(),
    };

    let result = run_one_shot(opts).await?;

    // Before the timeout / error exits below: a run that gave up still
    // produced a turn, and that turn is exactly what an eval wants to score.
    // stderr, because stdout is the agent's answer and callers parse it.
    if let Some(jid) = &cmd.trajectory {
        eprintln!(
            "[agent-task] trajectory: {}",
            crate::trajectory::chat_dir(jid).display()
        );
    }

    if result.timed_out {
        eprintln!(
            "[agent-task] timed out after {}ms (turns: {})",
            result.duration.as_millis(),
            result.turn_count
        );
        std::process::exit(124);
    }
    if result.errored {
        eprintln!(
            "[agent-task] session error: {}",
            result.error_message.as_deref().unwrap_or("unknown")
        );
        std::process::exit(1);
    }

    let final_text = result.text;

    match cmd.output {
        OutputFmt::Json => match try_parse_json(&final_text) {
            Some(value) => {
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "{}", serde_json::to_string(&value)?).ok();
            }
            None => {
                eprintln!("[agent-task] expected JSON output but got non-JSON");
                eprintln!("[agent-task] raw final text:");
                eprintln!("{}", final_text);
                std::process::exit(3);
            }
        },
        OutputFmt::Raw => {
            let joined = result.all_texts.join("\n---\n");
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{}", joined).ok();
        }
        OutputFmt::Text => {
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{}", final_text).ok();
        }
    }

    Ok(())
}

// ===== input handling =====

fn read_prompt_input(cmd: &AgentTaskCmd) -> Result<String> {
    if let Some(ref p) = cmd.prompt {
        return Ok(p.clone());
    }
    if let Some(ref f) = cmd.prompt_file {
        if f == "-" {
            return read_stdin();
        }
        return Ok(std::fs::read_to_string(resolve_user_path(f))?);
    }
    read_stdin()
}

fn read_stdin() -> Result<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return Ok(String::new());
    }
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

fn resolve_user_path(p: &str) -> PathBuf {
    if p == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    Path::new(p)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(p))
}

fn assert_dir_exists(dir: &Path, flag: &str) {
    match std::fs::metadata(dir) {
        Err(_) => {
            eprintln!("Error: {} path does not exist: {}", flag, dir.display());
            std::process::exit(2);
        }
        Ok(meta) if !meta.is_dir() => {
            eprintln!("Error: {} is not a directory: {}", flag, dir.display());
            std::process::exit(2);
        }
        _ => {}
    }
}

// ===== JSON parsing =====

fn try_parse_json(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Some(v);
    }
    // Try ```json ... ``` (or unlabeled fenced) block
    if let Some(fenced) = extract_fenced_block(trimmed) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&fenced) {
            return Some(v);
        }
    }
    // Try first {...} or [...] substring
    if let Some(span) = first_json_span(trimmed) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(span) {
            return Some(v);
        }
    }
    None
}

fn extract_fenced_block(s: &str) -> Option<String> {
    let start = s.find("```")?;
    let after_start = &s[start + 3..];
    // skip optional language tag
    let after_tag = match after_start.find('\n') {
        Some(nl) => &after_start[nl + 1..],
        None => after_start,
    };
    let end = after_tag.find("```")?;
    Some(after_tag[..end].to_string())
}

fn first_json_span(s: &str) -> Option<&str> {
    let first_brace = s.find('{');
    let first_bracket = s.find('[');
    let start = match (first_brace, first_bracket) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    let last_brace = s.rfind('}');
    let last_bracket = s.rfind(']');
    let end = match (last_brace, last_bracket) {
        (Some(a), Some(b)) => a.max(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    if end <= start {
        return None;
    }
    Some(&s[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_direct_parse() {
        let v = try_parse_json(r#"{"a":1}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn json_fenced_parse() {
        let txt = "Here is the result:\n```json\n{\"a\":2}\n```\nDone.";
        let v = try_parse_json(txt).unwrap();
        assert_eq!(v["a"], 2);
    }

    #[test]
    fn json_unfenced_extract() {
        let txt = "Result: {\"a\":3} trailing";
        let v = try_parse_json(txt).unwrap();
        assert_eq!(v["a"], 3);
    }

    #[test]
    fn json_returns_none_for_non_json() {
        assert!(try_parse_json("just text, no json").is_none());
        assert!(try_parse_json("").is_none());
    }

    #[test]
    fn first_json_span_handles_array() {
        assert_eq!(first_json_span("prefix [1,2,3] suffix"), Some("[1,2,3]"));
    }
}

//! `senclaw workflow` — list / inspect / run saved workflows.
//!
//! Port of the upstream `semaclaw workflow` CLI surface:
//!
//! ```text
//! senclaw workflow list
//! senclaw workflow show <name>
//! senclaw workflow run <name> -i topic=x -i depth=deep [--json]
//! senclaw workflow runs [--name <wf>] [--json]
//! ```
//!
//! `run` executes in-process (isolated agent sessions + shell scripts) and
//! prints per-step status, result previews, and observe output.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use clap::Subcommand;

use crate::agent::persona_registry::PersonaRegistry;
use crate::config::Config;
use crate::workflow::types::{RunStatus, StepStatus, WorkflowRun};
use crate::workflow::{WorkflowExecutor, WorkflowExecutorOpts, WorkflowRegistry, WorkflowRunStore};

#[derive(Subcommand, Debug, Clone)]
pub enum WorkflowCmd {
    /// List available workflow definitions
    List,
    /// Show one workflow definition (steps, inputs, dependencies)
    Show { name: String },
    /// Run a workflow and wait for completion
    Run {
        name: String,
        /// Run inputs as key=value (repeatable)
        #[arg(short = 'i', long = "input", value_name = "KEY=VALUE")]
        inputs: Vec<String>,
        /// Print the full run record as JSON
        #[arg(long)]
        json: bool,
    },
    /// List run history (newest first)
    Runs {
        /// Filter by workflow name
        #[arg(long)]
        name: Option<String>,
        /// Print records as JSON
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(cmd: WorkflowCmd) -> Result<()> {
    let cfg = Config::from_env();

    match cmd {
        WorkflowCmd::List => {
            let registry = WorkflowRegistry::new(cfg.paths.workflows_dir.clone());
            let defs = registry.list();
            if defs.is_empty() {
                println!(
                    "No workflows found in {}",
                    cfg.paths.workflows_dir.display()
                );
                return Ok(());
            }
            for d in defs {
                let desc = d.description.as_deref().unwrap_or("");
                println!("{:<24} {:>2} step(s)  {}", d.name, d.steps.len(), desc);
            }
        }

        WorkflowCmd::Show { name } => {
            let registry = WorkflowRegistry::new(cfg.paths.workflows_dir.clone());
            let Some(d) = registry.get(&name) else {
                bail!(
                    "workflow \"{name}\" not found in {}",
                    cfg.paths.workflows_dir.display()
                );
            };
            println!("name:        {}", d.name);
            if let Some(desc) = &d.description {
                println!("description: {desc}");
            }
            println!("file:        {}", d.file_path.display());
            if let Some(ws) = &d.workspace {
                println!("workspace:   {ws}");
            }
            if !d.inputs.is_empty() {
                println!("inputs:");
                for i in &d.inputs {
                    let req = if i.required { " (required)" } else { "" };
                    let def = i
                        .default
                        .as_deref()
                        .map(|v| format!(" [default: {v}]"))
                        .unwrap_or_default();
                    println!("  - {}{req}{def}", i.name);
                }
            }
            println!("steps:");
            for s in &d.steps {
                let deps = if s.depends_on.is_empty() {
                    String::new()
                } else {
                    format!("  ← {}", s.depends_on.join(", "))
                };
                let who = s.persona.as_deref().unwrap_or("shell");
                println!("  - {:<20} [{:?}/{who}]{deps}", s.id, s.kind);
            }
        }

        WorkflowCmd::Run { name, inputs, json } => {
            let inputs = parse_inputs(&inputs)?;
            let registry = WorkflowRegistry::new(cfg.paths.workflows_dir.clone());
            let Some(def) = registry.get(&name).cloned() else {
                bail!(
                    "workflow \"{name}\" not found in {}",
                    cfg.paths.workflows_dir.display()
                );
            };

            let persona_registry = Arc::new(Mutex::new(PersonaRegistry::new(
                cfg.paths.virtual_agents_dir.clone(),
            )));
            let store = Arc::new(WorkflowRunStore::new(cfg.paths.workflow_state_path.clone()));
            // Same runtime settings file as the daemon (LLM parallelism, retries).
            let settings_path = cfg
                .paths
                .workflow_state_path
                .with_file_name("workflow-settings.json");
            let settings = crate::workflow::LiveWorkflowSettings::new(
                &crate::workflow::WorkflowSettings::load(&settings_path),
            );
            let executor = WorkflowExecutor::new(WorkflowExecutorOpts {
                store,
                persona_registry,
                concurrency: None,
                on_update: None,
                workflow_data_dir: cfg.paths.workflow_data_dir.clone(),
                skills_extra_dirs: default_skills_dirs(&cfg),
                extra_mcp_servers: default_extra_mcp_servers(&cfg),
                shell_override: cfg.workflow_shell.clone(),
                settings,
            });

            eprintln!("Running workflow \"{name}\" …");
            let run = executor.run(&def, inputs, Some("cli".into())).await?;

            if json {
                println!("{}", serde_json::to_string_pretty(&run)?);
            } else {
                print_run(&run, true);
            }
            if run.status != RunStatus::Done {
                std::process::exit(1);
            }
        }

        WorkflowCmd::Runs { name, json } => {
            let store = WorkflowRunStore::new(cfg.paths.workflow_state_path.clone());
            let runs: Vec<WorkflowRun> = store
                .load()
                .into_iter()
                .filter(|r| name.as_deref().map_or(true, |n| r.workflow_name == n))
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&runs)?);
            } else if runs.is_empty() {
                println!("No runs recorded.");
            } else {
                for r in &runs {
                    print_run(r, false);
                    println!();
                }
            }
        }
    }

    Ok(())
}

fn parse_inputs(pairs: &[String]) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for p in pairs {
        let Some((k, v)) = p.split_once('=') else {
            bail!("invalid --input \"{p}\" (expected key=value)");
        };
        map.insert(k.trim().to_string(), v.to_string());
    }
    Ok(map)
}

fn status_icon(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Done => "✓",
        StepStatus::Failed => "✗",
        StepStatus::Skipped => "○",
        StepStatus::Running => "▸",
        StepStatus::Pending => "·",
    }
}

fn print_run(run: &WorkflowRun, detail: bool) {
    let status = serde_json::to_string(&run.status)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string();
    println!("{}  [{status}]  {}", run.id, run.created_at);
    for s in &run.steps {
        let mut line = format!("  {} {:<20}", status_icon(s.status), s.id);
        if let Some(err) = &s.error {
            line.push_str(&format!("  error: {err}"));
        } else if !s.result.is_empty() {
            let preview: String = s.result.chars().take(80).collect();
            let ellipsis = if s.result.chars().count() > 80 {
                "…"
            } else {
                ""
            };
            line.push_str(&format!("  {preview}{ellipsis}"));
        }
        println!("{line}");
        if detail {
            if let Some(obs) = &s.observe {
                if let Some(content) = &obs.content {
                    println!("    ── {} ──", obs.label);
                    for l in content.lines().take(20) {
                        println!("    {l}");
                    }
                } else if let Some(path) = &obs.artifact_path {
                    println!("    ── {} → {path}", obs.label);
                }
            }
        }
    }
}

/// MCP servers injected into agent-step sessions — browser-mcp, mirroring
/// VirtualWorkerPool, so web personas (web-scout, browser-agent…) actually
/// have their tools. The subprocess talks to the daemon's WS port; without a
/// running daemon the tools degrade gracefully.
pub fn default_extra_mcp_servers(cfg: &Config) -> Vec<crate::zen_core::McpServerConfig> {
    let helper_cfg = crate::mcp::helper::browser_mcp_config(cfg.ws_port, "workflow");
    vec![crate::zen_core::McpServerConfig {
        name: helper_cfg.name,
        command: helper_cfg.command,
        args: helper_cfg.args,
        env: helper_cfg.env,
        request_timeout_secs: None,
    }]
}

/// Skills dirs for agent steps: bundled + managed (mirrors what the daemon
/// hands to isolated sessions; `<workspace>/skills` is appended per run by
/// the executor).
pub fn default_skills_dirs(cfg: &Config) -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(b) = &cfg.paths.bundled_skills_dir {
        if b.is_dir() {
            dirs.push(b.to_string_lossy().to_string());
        }
    }
    if cfg.paths.managed_skills_dir.is_dir() {
        dirs.push(cfg.paths.managed_skills_dir.to_string_lossy().to_string());
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_inputs_splits_on_first_eq() {
        let m = parse_inputs(&["a=1".into(), "b=x=y".into()]).unwrap();
        assert_eq!(m["a"], "1");
        assert_eq!(m["b"], "x=y");
        assert!(parse_inputs(&["broken".into()]).is_err());
    }
}

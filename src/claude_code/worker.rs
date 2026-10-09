//! Runs a dispatched work item in Claude Code and decides its outcome from
//! evidence SenClaw gathers itself:
//!
//! 1. the worker session (Stop hook gates finishing on the verify command),
//! 2. an independent re-run of the verify command after the session ends,
//! 3. a read-only reviewer session over the worktree diff,
//! 4. on failure, feedback goes back into the same session (`--resume`) up to
//!    `maxFixRounds` times; then the item is blocked for a person.
//!
//! The model's own report only ever *lowers* the outcome (it may say
//! "blocked"); it can never mark work done on its own.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use app_space_sdk::dispatch::{McpServerSpec, Outcome, WorkItem};
use serde_json::{json, Value};

use super::registry;
use super::runner::{self, RunSpec, Session, StreamEvent};
use super::settings::{self, ClaudeCodeSettings};
use super::verify::{self, VerifyResult};
use crate::mcp::helper::McpServerConfig;

/// Assignee names routed to Claude Code instead of a SenClaw persona.
pub fn handles(assignee: Option<&str>) -> bool {
    matches!(
        assignee.map(|a| a.trim().to_ascii_lowercase()).as_deref(),
        Some("claude-code") | Some("claude")
    )
}

/// Largest diff handed to the reviewer.
const REVIEW_DIFF_BYTES: usize = 60 * 1024;

const WORKER_GUIDANCE: &str = "\
## SenClaw worker rules
- You are running a task dispatched by SenClaw. Work only inside the current directory.
- Post short progress notes with `kanban_comment` when the kanban tools are available.
- Do NOT close or block the card yourself: SenClaw verifies your work and closes it.
- Run the project's tests for what you changed. When a verification failure is reported back to \
you, fix the cause; never weaken or delete tests to make them pass.
- Commit your changes with git when you are done (clear conventional commit message).
- Your final answer is the structured result: status `done` when finished, `blocked` when you \
need a person (say exactly what you need in `summary`).";

fn result_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": { "type": "string", "enum": ["done", "blocked"] },
            "summary": { "type": "string" },
            "files_changed": { "type": "array", "items": { "type": "string" } },
            "tests_run": { "type": "array", "items": { "type": "string" } },
            "risks": { "type": "string" }
        },
        "required": ["status", "summary"]
    })
}

fn review_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "verdict": { "type": "string", "enum": ["pass", "changes_requested"] },
            "summary": { "type": "string" },
            "issues": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["verdict", "summary"]
    })
}

/// The worker's structured report.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub blocked: bool,
    pub summary: String,
}

/// Structured output when present; else a JSON object at the end of the text;
/// else the first line of the text, read as "done".
pub fn report_from(structured: Option<&Value>, text: &str) -> Report {
    let parsed = structured.cloned().or_else(|| {
        let start = text.rfind("{\"status\"").or_else(|| text.rfind('{'))?;
        serde_json::from_str::<Value>(text[start..].trim().trim_end_matches("```").trim()).ok()
    });
    match parsed {
        Some(v) if v.get("status").is_some() => Report {
            blocked: v["status"] == "blocked",
            summary: v["summary"].as_str().unwrap_or("").trim().to_string(),
        },
        _ => Report {
            blocked: false,
            summary: text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string(),
        },
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Review {
    pub pass: bool,
    pub summary: String,
    pub issues: Vec<String>,
}

pub fn review_from(structured: Option<&Value>, text: &str) -> Review {
    let v = structured.cloned().or_else(|| {
        let start = text.rfind('{')?;
        serde_json::from_str::<Value>(&text[start..]).ok()
    });
    match v {
        Some(v) => Review {
            pass: v["verdict"] == "pass",
            summary: v["summary"].as_str().unwrap_or("").to_string(),
            issues: v["issues"]
                .as_array()
                .map(|a| a.iter().filter_map(|i| i.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        },
        // An unreadable verdict is not a pass.
        None => Review { pass: false, summary: "reviewer gave no verdict".into(), issues: vec![] },
    }
}

/// Feedback prompt for the next fix round.
pub fn feedback(verify: Option<&VerifyResult>, review: Option<&Review>) -> String {
    let mut parts = vec!["SenClaw checked your work and it is not done yet.".to_string()];
    if let Some(v) = verify.filter(|v| !v.passed) {
        parts.push(format!("Verification failed:\n{}", v.report()));
    }
    if let Some(r) = review.filter(|r| !r.pass) {
        let mut s = format!("Code review requested changes: {}", r.summary);
        for i in &r.issues {
            s.push_str(&format!("\n- {i}"));
        }
        parts.push(s);
    }
    parts.push("Fix these, commit, and give the structured result again.".into());
    parts.join("\n\n")
}

pub struct ClaudeCodeWorker {
    config: Arc<crate::config::Config>,
    db: Option<Arc<crate::db::Db>>,
    /// Daemon base URL the Stop hook calls (`http://127.0.0.1:18788`).
    hook_base: String,
    /// Scratch root for per-run config files.
    run_root: PathBuf,
}

impl ClaudeCodeWorker {
    pub fn new(config: Arc<crate::config::Config>, db: Option<Arc<crate::db::Db>>, hook_base: String) -> Self {
        let run_root = crate::util::paths::senclaw_home().join("claude-code").join("runs");
        Self { config, db, hook_base, run_root }
    }

    fn settings(&self) -> ClaudeCodeSettings {
        settings::load(&self.config.paths.global_config_path)
    }

    /// Run `item` in `working_dir`. `timeout` bounds the whole item, all rounds included.
    pub async fn execute(&self, item: &WorkItem, working_dir: &str, timeout: Duration) -> Outcome {
        let st = self.settings();
        if super::binary_version(&st.binary).await.is_none() {
            return Outcome::Blocked {
                reason: format!(
                    "claude-code: `{}` is not runnable — install Claude Code and sign in, or set claudeCode.binary",
                    st.binary
                ),
            };
        }
        let verify_cmd = verify::configured(st.verify_command.as_deref());
        let (run_id, token) = registry::register(registry::NewRun {
            item_id: item.id.clone(),
            cwd: working_dir.to_string(),
            verify_command: verify_cmd.clone(),
            verify_timeout_secs: st.verify_timeout_secs,
            max_stop_blocks: st.max_stop_blocks,
            mcp_chat_jid: st.mcp_chat_jid.clone(),
        });
        let outcome = self
            .drive(&st, item, Path::new(working_dir), timeout, &run_id, &token, verify_cmd)
            .await;
        let _ = std::fs::remove_dir_all(self.run_root.join(&run_id));
        let phase = match &outcome {
            Outcome::Completed { .. } => "done",
            Outcome::Blocked { .. } => "blocked",
            Outcome::Failed { .. } | Outcome::TimedOut => "failed",
        };
        registry::finish(&run_id, phase);
        outcome
    }

    /// The MCP servers the worker session gets, the tool allow/deny additions,
    /// and the chat-scope note for the system prompt.
    fn mcp_for(&self, st: &ClaudeCodeSettings, item: &WorkItem) -> (Value, Vec<String>, Vec<String>, Option<String>) {
        let mut stdio: Vec<McpServerConfig> = Vec::new();
        let mut http: Vec<(String, String)> = Vec::new();
        for spec in &item.mcp {
            match spec {
                McpServerSpec::Stdio { name, command, args, env } => {
                    let mut c = McpServerConfig::new(name, "");
                    c.command = command.clone();
                    c.args = args.clone();
                    c.env = env.clone();
                    stdio.push(c);
                }
                McpServerSpec::Http { name, url } => http.push((name.clone(), url.clone())),
            }
        }
        let mut scope = None;
        if let Some(db) = &self.db {
            if let Some(g) = super::senclaw_mcp::resolve_group(db, st.mcp_chat_jid.as_deref()) {
                stdio.push(super::senclaw_mcp::core_server(&self.config, &g));
                scope = Some(super::senclaw_mcp::scope_instructions(&g));
            }
        }
        let mut json = super::senclaw_mcp::claude_mcp_json(&stdio);
        for (name, url) in &http {
            json["mcpServers"][name] = json!({ "type": "http", "url": url });
        }
        let names: Vec<String> = stdio.iter().map(|c| c.name.clone()).chain(http.iter().map(|h| h.0.clone())).collect();
        let allow = names.iter().map(|n| format!("mcp__{n}")).collect();
        // The source's own "close the item" tools: SenClaw closes it after verifying.
        let deny = names
            .iter()
            .flat_map(|n| [format!("mcp__{n}__kanban_complete"), format!("mcp__{n}__kanban_block")])
            .collect();
        (json, allow, deny, scope)
    }

    #[allow(clippy::too_many_arguments)]
    async fn drive(
        &self,
        st: &ClaudeCodeSettings,
        item: &WorkItem,
        cwd: &Path,
        timeout: Duration,
        run_id: &str,
        token: &str,
        verify_cmd: Option<String>,
    ) -> Outcome {
        let deadline = Instant::now() + timeout;
        let (mcp_config, allow_mcp, deny, scope) = self.mcp_for(st, item);
        let mut allowed = st.allowed_tools.clone();
        allowed.extend(allow_mcp);
        // The source's own guidance is not used: it tells its workers to close
        // the item themselves, which this path forbids (SenClaw closes it).
        let mut system = format!(
            "{WORKER_GUIDANCE}\n- This is SenClaw work item `{id}`; with the kanban tools, read it via \
             `kanban_show(card_id={id})`.",
            id = item.id
        );
        if let Some(note) = scope {
            system.push_str(&format!("\n- {note}"));
        }
        let session_id = uuid::Uuid::new_v4().to_string();
        registry::update(run_id, |e| e.session_id = Some(session_id.clone()));
        let run_dir = self.run_root.join(run_id);
        let mut prompt = item.prompt.clone();
        let mut total_cost = 0.0;
        let mut verify_note = String::from("not configured");
        let mut review_note = String::from("skipped");

        for round in 0..=st.max_fix_rounds {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Outcome::TimedOut;
            }
            registry::update(run_id, |e| {
                e.phase = "running".into();
                e.fix_round = round;
                // Each round gets its own Stop-hook budget.
                e.stop_blocks = 0;
            });
            let spec = RunSpec {
                binary: st.binary.clone(),
                cwd: cwd.to_path_buf(),
                prompt: prompt.clone(),
                session: if round == 0 { Session::New(session_id.clone()) } else { Session::Resume(session_id.clone()) },
                append_system_prompt: Some(system.clone()),
                permission_mode: st.permission_mode.clone(),
                allowed_tools: allowed.clone(),
                disallowed_tools: deny.clone(),
                model: st.model.clone(),
                max_turns: st.max_turns,
                max_budget_usd: st.max_budget_usd,
                json_schema: Some(result_schema()),
                mcp_config: mcp_config.clone(),
                settings: Some(super::hooks::stop_hook_settings(&self.hook_base, token, st.verify_timeout_secs + 60)),
                run_dir: run_dir.clone(),
                timeout: remaining,
            };
            let res = match runner::run(&spec, |ev| progress(run_id, ev)).await {
                Ok(r) => r,
                Err(e) => return Outcome::Failed { error: format!("claude-code: {e:#}") },
            };
            if let Some(r) = &res.result {
                total_cost += r.cost_usd;
                let c = total_cost;
                registry::update(run_id, |e| e.cost_usd = c);
            }
            if res.timed_out {
                return Outcome::TimedOut;
            }
            if !res.succeeded() {
                return Outcome::Failed { error: res.error_text() };
            }
            let r = res.result.as_ref().expect("succeeded implies a result");
            let report = report_from(r.structured.as_ref(), &r.result);
            if report.blocked {
                return Outcome::Blocked { reason: format!("claude-code: {}", report.summary) };
            }

            // Independent checks.
            let v = match &verify_cmd {
                Some(cmd) => {
                    registry::update(run_id, |e| e.phase = "verifying".into());
                    let left = deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(st.verify_timeout_secs));
                    let v = verify::run(cmd, cwd, left).await;
                    verify_note = if v.passed { format!("passed (`{cmd}`)") } else { format!("failed (`{cmd}`)") };
                    Some(v)
                }
                None => None,
            };
            let review = if st.review && v.as_ref().is_none_or(|v| v.passed) {
                match self.review(st, item, cwd, run_dir.clone(), deadline, run_id, &mut total_cost).await {
                    ReviewStep::Skipped(why) => {
                        review_note = format!("skipped ({why})");
                        None
                    }
                    ReviewStep::Done(rv) => {
                        review_note = format!("{} — {}", if rv.pass { "pass" } else { "changes requested" }, rv.summary);
                        Some(rv)
                    }
                    ReviewStep::Error(e) => return Outcome::Blocked { reason: format!("claude-code review could not run: {e}") },
                }
            } else {
                None
            };

            // With no verify command and nothing to review, nothing but the
            // model's own word says the work is done — a person decides.
            if v.is_none() && review.is_none() {
                return Outcome::Blocked {
                    reason: format!(
                        "review-required: claude-code reports done, but no independent check ran \
                         (no claudeCode.verifyCommand; review {review_note}). {}",
                        report.summary
                    ),
                };
            }
            let ok = v.as_ref().is_none_or(|v| v.passed) && review.as_ref().is_none_or(|r| r.pass);
            if ok {
                let summary = format!(
                    "{}\n\nverify: {verify_note}\nreview: {review_note}\nclaude-code session `{session_id}`, {} fix round(s), cost ${total_cost:.2}",
                    if report.summary.is_empty() { "claude-code finished" } else { &report.summary },
                    round
                );
                return Outcome::Completed {
                    summary,
                    metadata: json!({
                        "claude_code": {
                            "session_id": session_id,
                            "fix_rounds": round,
                            "cost_usd": total_cost,
                            "verify": verify_note,
                            "review": review_note,
                        }
                    }),
                };
            }
            let fb = feedback(v.as_ref(), review.as_ref());
            if round == st.max_fix_rounds {
                return Outcome::Blocked {
                    reason: format!(
                        "claude-code: checks still failing after {} fix round(s) (session `{session_id}`).\n\n{fb}",
                        st.max_fix_rounds
                    ),
                };
            }
            prompt = fb;
        }
        unreachable!("the loop returns on its last round")
    }

    #[allow(clippy::too_many_arguments)]
    async fn review(
        &self,
        st: &ClaudeCodeSettings,
        item: &WorkItem,
        cwd: &Path,
        run_dir: PathBuf,
        deadline: Instant,
        run_id: &str,
        total_cost: &mut f64,
    ) -> ReviewStep {
        let d = match crate::worktree::diff(cwd) {
            Ok(d) => d,
            Err(_) => return ReviewStep::Skipped("not a SenClaw worktree".into()),
        };
        if d.files.is_empty() && d.diff.trim().is_empty() {
            return ReviewStep::Skipped("no changes".into());
        }
        registry::update(run_id, |e| e.phase = "reviewing".into());
        let mut diff = d.diff.clone();
        if diff.len() > REVIEW_DIFF_BYTES {
            diff.truncate(diff.floor_char_boundary(REVIEW_DIFF_BYTES));
            diff.push_str("\n… (diff truncated; read the files for the rest)");
        }
        let prompt = format!(
            "Review this change against its task. Read files as needed; do not modify anything.\n\n\
             ## Task\n{}\n\n## Diff (base `{}` → branch `{}`)\n{}\n```diff\n{}\n```\n\n\
             Pass only if the change does what the task asks, has no evident bugs or regressions, and \
             tests were not weakened. Give your verdict as the structured result.",
            item.prompt, d.base, d.branch, d.stat, diff
        );
        let spec = RunSpec {
            binary: st.binary.clone(),
            cwd: cwd.to_path_buf(),
            prompt,
            session: Session::New(uuid::Uuid::new_v4().to_string()),
            append_system_prompt: Some("You are a strict, read-only code reviewer for SenClaw.".into()),
            permission_mode: "plan".into(),
            allowed_tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
            disallowed_tools: vec!["Edit".into(), "Write".into(), "Bash".into()],
            model: st.model.clone(),
            max_turns: 30,
            max_budget_usd: st.max_budget_usd,
            json_schema: Some(review_schema()),
            mcp_config: json!({ "mcpServers": {} }),
            settings: None,
            run_dir,
            timeout: deadline.saturating_duration_since(Instant::now()),
        };
        match runner::run(&spec, |_| {}).await {
            Ok(res) if res.succeeded() => {
                let r = res.result.as_ref().expect("succeeded implies a result");
                *total_cost += r.cost_usd;
                ReviewStep::Done(review_from(r.structured.as_ref(), &r.result))
            }
            Ok(res) => ReviewStep::Error(res.error_text()),
            Err(e) => ReviewStep::Error(format!("{e:#}")),
        }
    }
}

enum ReviewStep {
    Skipped(String),
    Done(Review),
    Error(String),
}

fn progress(run_id: &str, ev: &StreamEvent) {
    let note = match ev {
        StreamEvent::ToolUse { name } => format!("tool: {name}"),
        StreamEvent::Text(t) => t.lines().next().unwrap_or("").chars().take(160).collect(),
        _ => return,
    };
    registry::update(run_id, |e| e.last_activity = note);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_only_the_claude_assignees() {
        assert!(handles(Some("claude-code")));
        assert!(handles(Some(" Claude ")));
        assert!(!handles(Some("coder")));
        assert!(!handles(None));
    }

    #[test]
    fn report_prefers_structured_then_trailing_json_then_text() {
        let s = json!({"status": "blocked", "summary": "need the API key name"});
        assert_eq!(report_from(Some(&s), "ignored"), Report { blocked: true, summary: "need the API key name".into() });
        let t = "did it\n{\"status\":\"done\",\"summary\":\"added tests\"}";
        assert_eq!(report_from(None, t), Report { blocked: false, summary: "added tests".into() });
        assert_eq!(report_from(None, "\nall good\nmore"), Report { blocked: false, summary: "all good".into() });
    }

    #[test]
    fn an_unreadable_review_is_not_a_pass() {
        assert!(!review_from(None, "looks fine").pass);
        let v = json!({"verdict": "changes_requested", "summary": "s", "issues": ["off by one"]});
        let r = review_from(Some(&v), "");
        assert!(!r.pass && r.issues == vec!["off by one".to_string()]);
        assert!(review_from(Some(&json!({"verdict": "pass", "summary": "ok"})), "").pass);
    }

    #[test]
    fn feedback_names_every_failed_check() {
        let v = VerifyResult { command: "cargo test".into(), passed: false, exit_code: Some(101), timed_out: false, output_tail: "panicked".into() };
        let r = Review { pass: false, summary: "missing edge case".into(), issues: vec!["empty input".into()] };
        let f = feedback(Some(&v), Some(&r));
        assert!(f.contains("panicked") && f.contains("missing edge case") && f.contains("- empty input"));
        let passed = VerifyResult { passed: true, ..v };
        assert!(!feedback(Some(&passed), None).contains("Verification failed"));
    }
}

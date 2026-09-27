//! The tool-call gate: before an agent's shell command prompts the person,
//! ask the decision engine what the command does, and approve it once when the
//! answer is confident that it only reads, tests or edits inside the project.
//!
//! It plugs into [`PermissionBridge`](crate::agent::permission_bridge::PermissionBridge)
//! as a [`PermissionGate`], so it only ever sees requests that were about to
//! prompt — after the engine's safe-command list, saved grants and the
//! auto-accept rules. It can only skip a prompt, never deny: a command on the
//! risk list, a low score, an engine error or a timeout all leave the prompt.
//!
//! Modes (Settings → Decision): `off` (default), `shadow` (judge and record,
//! still prompt — the person's answer is logged next to the gate's), `on`.
//! The shell logic itself is [`shell`], ported from the Laya-jev demo.

pub mod shell;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;

use crate::agent::permission_bridge::{GateCall, PermissionGate};
use crate::db::tool_gate::GateLogEntry;
use crate::db::Db;
use crate::decision::client;
use crate::decision::json::Json;
use crate::decision::settings::{DecisionSettings, FeatureMode, GateQuestions};
use crate::decision::types::{Answers, AskRequest};
use crate::gateway::group_manager::load_decision_settings;
use crate::runtime::manager::RuntimeManager;

use shell::QuestionSet;

/// How long a prompt may wait on the gate, a cold load of a checkpoint
/// included (~3–4 s). Past it the prompt shows; the load carries on.
pub const GATE_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Allow,
    Ask,
}

/// Where the verdict came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// The static risk list — never asked.
    Risky,
    /// The decision engine answered.
    Engine,
    /// The engine failed or timed out.
    Error,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Allow => "allow",
            Outcome::Ask => "ask",
        }
    }
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Stage::Risky => "risky",
            Stage::Engine => "engine",
            Stage::Error => "error",
        }
    }
}

/// One judgment, with everything the Settings page shows about it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    pub command: String,
    pub outcome: Outcome,
    pub stage: Stage,
    pub reason: String,
    pub risky: Option<shell::RiskyMatch>,
    pub questions: QuestionSet,
    pub checks: Vec<shell::Check>,
    /// The kind the model chose (`laya` questions).
    pub choice: Option<String>,
    pub model: Option<String>,
    pub engine: Option<String>,
    pub latency_ms: Option<f64>,
    /// What was sent and what came back — in order.
    pub state: Option<Json>,
    pub asked: Option<Json>,
    pub answers: Option<Answers>,
}

/// The questions for these settings. `Auto` used to follow the local/online
/// backend choice; that choice now lives in the `sen-sysone` runtime, not the
/// daemon, so `Auto` resolves to the Laya shape — the safer blind default
/// (see `GateQuestions::Auto`'s own doc).
pub fn question_set(settings: &DecisionSettings) -> QuestionSet {
    match settings.gate.questions {
        GateQuestions::Laya | GateQuestions::Auto => QuestionSet::Laya,
        GateQuestions::Cookbook => QuestionSet::Cookbook,
    }
}

/// Judge one shell command. Never fails: a failure is a verdict to ask.
pub async fn judge_command(settings: &DecisionSettings, runtime: &RuntimeManager, command: &str, project: &str) -> Verdict {
    let set = question_set(settings);
    let base = |outcome, stage, reason: String| Verdict {
        command: command.to_string(),
        outcome,
        stage,
        reason,
        risky: None,
        questions: set,
        checks: Vec::new(),
        choice: None,
        model: None,
        engine: None,
        latency_ms: None,
        state: None,
        asked: None,
        answers: None,
    };
    if let Some(m) = shell::risky_match(command) {
        let reason = format!("{}: `{}`", m.why, m.part);
        return Verdict {
            risky: Some(m),
            ..base(Outcome::Ask, Stage::Risky, reason)
        };
    }

    let (state, asked) = shell::request(set, command, project);
    let request = AskRequest {
        backend: None,
        model: None,
        state: state.clone(),
        questions: asked.clone(),
    };
    let started = Instant::now();
    let answered = tokio::time::timeout(GATE_TIMEOUT, client::ask(runtime, &request)).await;
    let latency_ms = (started.elapsed().as_secs_f64() * 10_000.0).round() / 10.0;
    let with_request = |v: Verdict| Verdict {
        state: Some(state.clone()),
        asked: Some(asked.clone()),
        latency_ms: Some(latency_ms),
        ..v
    };
    let response = match answered {
        Err(_) => {
            return with_request(base(
                Outcome::Ask,
                Stage::Error,
                format!("the decision engine did not answer within {} s", GATE_TIMEOUT.as_secs()),
            ))
        }
        Ok(Err(e)) => {
            return with_request(base(Outcome::Ask, Stage::Error, format!("the decision engine failed: {e}")))
        }
        Ok(Ok(r)) => r,
    };
    let answers_json = serde_json::to_value(&response.answers).unwrap_or_default();
    let verdict = match shell::checks(set, &answers_json) {
        Err(e) => base(Outcome::Ask, Stage::Error, e),
        Ok((checks, choice)) => {
            let (allow, reason) = shell::decide(&checks, settings.gate.approve_at);
            Verdict {
                checks,
                choice,
                ..base(if allow { Outcome::Allow } else { Outcome::Ask }, Stage::Engine, reason)
            }
        }
    };
    Verdict {
        model: Some(response.model),
        engine: Some(response.engine),
        answers: Some(response.answers),
        ..with_request(verdict)
    }
}

/// The shell command a permission request is about (`Bash` only).
fn bash_command<'a>(tool_name: &str, content: &'a serde_json::Value) -> Option<&'a str> {
    if tool_name != "Bash" {
        return None;
    }
    content
        .get("command")
        .and_then(|v| v.as_str())
        .or_else(|| content.as_str())
        .filter(|c| !c.trim().is_empty())
}

/// The gate as the permission bridge sees it: settings read per request (a
/// change in Settings applies to the next prompt), verdicts logged.
pub struct ToolGate {
    config_path: PathBuf,
    runtime: Arc<RuntimeManager>,
    db: Arc<Db>,
    /// The working directory of a chat — the cookbook's `project`.
    cwd_for: Box<dyn Fn(&str) -> Option<String> + Send + Sync>,
}

impl ToolGate {
    pub fn new(
        config_path: PathBuf,
        runtime: Arc<RuntimeManager>,
        db: Arc<Db>,
        cwd_for: Box<dyn Fn(&str) -> Option<String> + Send + Sync>,
    ) -> ToolGate {
        ToolGate {
            config_path,
            runtime,
            db,
            cwd_for,
        }
    }
}

#[async_trait]
impl PermissionGate for ToolGate {
    fn wants(&self, tool_name: &str, content: &serde_json::Value) -> bool {
        bash_command(tool_name, content).is_some() && load_decision_settings(&self.config_path).gate.mode != FeatureMode::Off
    }

    async fn judge(&self, tool_name: &str, content: &serde_json::Value, chat_jid: &str) -> GateCall {
        let settings = load_decision_settings(&self.config_path);
        let mode = settings.gate.mode;
        let Some(command) = bash_command(tool_name, content).filter(|_| mode != FeatureMode::Off) else {
            return GateCall::default();
        };
        let project = (self.cwd_for)(chat_jid).unwrap_or_default();
        let v = judge_command(&settings, &self.runtime, command, &project).await;
        let applied = mode == FeatureMode::On && v.outcome == Outcome::Allow;
        tracing::info!(
            "[gate] `{command}` → {} ({}){}",
            v.outcome.as_str(),
            v.reason,
            if applied { ", prompt skipped" } else { "" }
        );
        let entry = GateLogEntry {
            chat_jid,
            tool: tool_name,
            command,
            mode: if mode == FeatureMode::On { "on" } else { "shadow" },
            outcome: v.outcome.as_str(),
            applied,
            stage: v.stage.as_str(),
            reason: &v.reason,
            p: v.checks.first().map(|c| c.p),
            choice: v.choice.as_deref(),
            questions: match v.questions {
                QuestionSet::Laya => "laya",
                QuestionSet::Cookbook => "cookbook",
            },
            model: v.model.as_deref(),
            engine: v.engine.as_deref(),
            latency_ms: v.latency_ms,
        };
        let log_id = self
            .db
            .insert_gate_log(&entry)
            .map_err(|e| tracing::warn!("[gate] could not record the verdict: {e:#}"))
            .ok();
        GateCall { allow: applied, log_id }
    }

    fn record_answer(&self, log_id: i64, option_key: &str) {
        if let Err(e) = self.db.set_gate_human(log_id, option_key) {
            tracing::warn!("[gate] could not record the answer: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::manager::{RuntimeManager, RuntimeManagerConfig};

    fn empty_runtime_manager(tmp: &std::path::Path) -> Arc<RuntimeManager> {
        RuntimeManager::new(RuntimeManagerConfig {
            runtimes_dir: tmp.join("runtimes"),
            runtime_data_dir: tmp.join("runtime-data"),
            runtime_logs_dir: tmp.join("logs"),
            bundled_dir: None,
            local_models_dir: tmp.join("local-models"),
            config_path: tmp.join("config.json"),
            home: tmp.to_path_buf(),
            index_url: "file:///dev/null".to_string(),
        })
    }

    #[test]
    fn auto_resolves_to_laya_and_an_explicit_choice_is_kept() {
        let mut s = DecisionSettings::default();
        assert_eq!(question_set(&s), QuestionSet::Laya);
        s.gate.questions = GateQuestions::Cookbook;
        assert_eq!(question_set(&s), QuestionSet::Cookbook);
        s.gate.questions = GateQuestions::Laya;
        assert_eq!(question_set(&s), QuestionSet::Laya);
    }

    #[test]
    fn only_bash_with_a_command_is_judged() {
        assert_eq!(bash_command("Bash", &serde_json::json!({"command": "ls"})), Some("ls"));
        assert_eq!(bash_command("Bash", &serde_json::json!({"command": "  "})), None);
        assert_eq!(bash_command("Write", &serde_json::json!({"command": "ls"})), None);
    }

    #[tokio::test]
    async fn a_risky_command_is_never_sent_and_an_unavailable_engine_asks() {
        let s = DecisionSettings::default();
        let tmp = tempfile::tempdir().unwrap();
        let runtime = empty_runtime_manager(tmp.path());
        let v = judge_command(&s, &runtime, "git push --force origin main", "/p").await;
        assert_eq!((v.outcome, v.stage), (Outcome::Ask, Stage::Risky));
        assert!(v.state.is_none(), "nothing was sent");

        // No decision runtime installed: the prompt stays, never a hard failure.
        let v = judge_command(&s, &runtime, "bun test", "/p").await;
        assert_eq!((v.outcome, v.stage), (Outcome::Ask, Stage::Error));
        assert!(v.state.is_some());
        assert!(v.engine.is_none());
    }
}

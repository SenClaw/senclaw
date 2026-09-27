//! Typed decisions — the daemon-owned control plane only.
//!
//! Model management, `/ask` and backend settings moved to the `sen-sysone`
//! runtime; the daemon proxies them (`src/runtime/proxy.rs`,
//! `docs/runtime-protocol.md` §5.2). What stays here:
//!
//!   GET    /api/decision/gate           — tool-call gate: settings, stats, recent verdicts
//!   PUT    /api/decision/gate           — `{ mode, approveAt, questions }`
//!   POST   /api/decision/gate/check     — `{ command }` or `{ commands }`: judge, do not log
//!   GET    /api/decision/skills         — pre-skill router: mode, stats, recent turns
//!   PUT    /api/decision/skills         — `{ mode }`
//!   POST   /api/decision/skills/check   — `{ prompt }`: route it, do not log
//!
//! The logic lives in [`crate::decision`]; this file maps it onto HTTP.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::decision::gate::{self, shell};
use crate::decision::settings::{DecisionSettings, GateSettings, SkillRouteSettings};
use crate::decision::skill_route;
use crate::gateway::group_manager::{load_decision_settings, save_decision_settings};

use super::core::{AppError, UiState};

fn internal(e: impl std::fmt::Display) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad(msg: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.into())
}

#[derive(Deserialize)]
pub(crate) struct GateQuery {
    #[serde(default)]
    limit: Option<usize>,
}

/// The gate's settings, what they resolve to, and how it has done so far.
fn gate_view(s: &UiState, settings: &DecisionSettings, limit: usize) -> Result<Value, AppError> {
    let (stats, log) = match &s.db {
        Some(db) => (
            serde_json::to_value(db.gate_stats().map_err(internal)?).map_err(internal)?,
            serde_json::to_value(db.list_gate_log(limit).map_err(internal)?).map_err(internal)?,
        ),
        None => (Value::Null, json!([])),
    };
    Ok(json!({
        "gate": settings.gate,
        "questionSet": gate::question_set(settings),
        "timeoutSecs": gate::GATE_TIMEOUT.as_secs(),
        "samples": shell::SAMPLE_COMMANDS,
        "stats": stats,
        "log": log,
    }))
}

pub(crate) async fn decision_gate_get(
    State(s): State<Arc<UiState>>,
    Query(q): Query<GateQuery>,
) -> Result<Json<Value>, AppError> {
    let settings = load_decision_settings(&s.config.paths.global_config_path);
    Ok(Json(gate_view(&s, &settings, q.limit.unwrap_or(50).min(500))?))
}

/// Changes only the gate, so it cannot race the "How it runs" form the
/// `sen-sysone` proxy serves for backend/local/online.
pub(crate) async fn decision_gate_put(
    State(s): State<Arc<UiState>>,
    Json(gate): Json<GateSettings>,
) -> Result<Json<Value>, AppError> {
    let mut settings = load_decision_settings(&s.config.paths.global_config_path);
    settings.gate = gate;
    let settings = settings.validated();
    save_decision_settings(&s.config.paths.global_config_path, &settings).map_err(internal)?;
    let mut view = gate_view(&s, &settings, 50)?;
    view["ok"] = Value::Bool(true);
    Ok(Json(view))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GateCheckBody {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    commands: Option<Vec<String>>,
    /// Try settings before saving them.
    #[serde(default)]
    approve_at: Option<f64>,
    #[serde(default)]
    questions: Option<crate::decision::settings::GateQuestions>,
}

/// Judge commands as the gate would, without logging them or asking anyone:
/// the "try a command" box and the sample run.
pub(crate) async fn decision_gate_check(
    State(s): State<Arc<UiState>>,
    Json(body): Json<GateCheckBody>,
) -> Result<Json<Value>, AppError> {
    let mut settings = load_decision_settings(&s.config.paths.global_config_path);
    if let Some(at) = body.approve_at {
        settings.gate.approve_at = at;
    }
    if let Some(q) = body.questions {
        settings.gate.questions = q;
    }
    let settings = settings.validated();
    let commands: Vec<String> = body
        .commands
        .unwrap_or_default()
        .into_iter()
        .chain(body.command)
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    if commands.is_empty() {
        return Err(bad("give a `command` or `commands` to check"));
    }
    if commands.len() > 50 {
        return Err(bad("at most 50 commands at a time"));
    }
    let Some(runtime) = s.runtime_manager.as_ref() else {
        return Err(AppError(StatusCode::SERVICE_UNAVAILABLE, "the runtime manager is not wired".into()));
    };
    let mut verdicts = Vec::with_capacity(commands.len());
    // One at a time: on a warm engine each is tens of milliseconds, and the
    // first may be starting the runtime for the rest.
    for command in &commands {
        verdicts.push(gate::judge_command(&settings, runtime, command, "").await);
    }
    Ok(Json(json!({ "questionSet": gate::question_set(&settings), "verdicts": verdicts })))
}

fn skills_view(s: &UiState, settings: &DecisionSettings, limit: usize) -> Result<Value, AppError> {
    let (stats, log) = match &s.db {
        Some(db) => (
            serde_json::to_value(db.skill_route_stats().map_err(internal)?).map_err(internal)?,
            serde_json::to_value(db.list_skill_route_log(limit).map_err(internal)?).map_err(internal)?,
        ),
        None => (Value::Null, json!([])),
    };
    Ok(json!({
        "skills": settings.skills,
        "preTriggerSkill": crate::gateway::group_manager::get_pre_trigger_skill_enabled(&s.config.paths.global_config_path),
        "timeoutMs": skill_route::ROUTE_TIMEOUT.as_millis() as u64,
        "candidates": skill_route::CANDIDATES,
        "stats": stats,
        "log": log,
    }))
}

pub(crate) async fn decision_skills_get(
    State(s): State<Arc<UiState>>,
    Query(q): Query<GateQuery>,
) -> Result<Json<Value>, AppError> {
    let settings = load_decision_settings(&s.config.paths.global_config_path);
    Ok(Json(skills_view(&s, &settings, q.limit.unwrap_or(50).min(500))?))
}

pub(crate) async fn decision_skills_put(
    State(s): State<Arc<UiState>>,
    Json(skills): Json<SkillRouteSettings>,
) -> Result<Json<Value>, AppError> {
    let mut settings = load_decision_settings(&s.config.paths.global_config_path);
    settings.skills = skills;
    save_decision_settings(&s.config.paths.global_config_path, &settings).map_err(internal)?;
    let mut view = skills_view(&s, &settings, 50)?;
    view["ok"] = Value::Bool(true);
    Ok(Json(view))
}

#[derive(Deserialize)]
pub(crate) struct SkillsCheckBody {
    prompt: String,
}

/// Route one request as the router would — with the skills the engines load —
/// without logging it or touching any chat.
pub(crate) async fn decision_skills_check(
    State(s): State<Arc<UiState>>,
    Json(body): Json<SkillsCheckBody>,
) -> Result<Json<skill_route::RouteReport>, AppError> {
    let prompt = body.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(bad("give a `prompt` to route"));
    }
    let config = Arc::clone(&s.config);
    let cards = tokio::task::spawn_blocking(move || {
        let disabled = crate::skills::disabled::read_disabled_skills();
        crate::skills::scan::load_all_local_skills(&config)
            .into_iter()
            .filter(|sk| sk.eligible && !disabled.contains(&sk.name))
            .filter(|sk| !sk.metadata.disable_model_invocation)
            .filter(|sk| sk.metadata.use_mode != crate::skills::SkillUseMode::Always)
            .map(|sk| crate::skills::matching::SkillCard {
                name: sk.metadata.name.clone(),
                description: sk.metadata.description.clone(),
                when_to_use: sk.metadata.when_to_use.clone(),
                triggers: sk.metadata.triggers.clone(),
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(internal)?;
    let Some(runtime) = s.runtime_manager.as_ref() else {
        return Err(AppError(StatusCode::SERVICE_UNAVAILABLE, "the runtime manager is not wired".into()));
    };
    let pre_trigger = crate::gateway::group_manager::get_pre_trigger_skill_enabled(&s.config.paths.global_config_path);
    Ok(Json(skill_route::route(runtime, &prompt, &cards, pre_trigger).await))
}

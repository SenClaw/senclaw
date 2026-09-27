//! The decision ladder (§1): `rule → Jev → LLM → human`. The "rule" tier is
//! whatever deterministic check the caller already runs before reaching here
//! (the shell risk list in [`super::policy_gate`], an explicit `#skill`) —
//! this module is the `Jev` rung: given a [`super::registry::Spec`], decide
//! whether to ask it at all (JEV_OFF, the spec's own mode, the shadow
//! opt-in), ask it, and turn the answer into one [`Decision`] the trace can
//! record without ever holding the state text.
//!
//! "LLM" and "human" are not separate code paths here: `Band::Fallback` and
//! `Band::Review` both mean "do not act on this answer" — the caller's
//! existing behaviour (ask the LLM, prompt the human) already **is** the
//! fallback, precisely because nothing about it needs to change by default.

use std::time::Instant;

use serde::Serialize;

use crate::decision::client;
use crate::decision::json::Json;
use crate::decision::types::AskRequest;
use crate::runtime::manager::RuntimeManager;

use super::registry::{Band, Spec, SpecMode};
use super::ControlPlaneSettings;

/// Bounded the same way the pre-skill router is: a turn must never wait on a
/// cold Laya load past this, and a shadow call that times out just logs a
/// timeout — nothing downstream is waiting on it.
pub const SPEC_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecidedBy {
    Jev,
}

/// One decision, safe to put in the trace verbatim: `spec`, `answer`,
/// `confidence` and `band` are metadata — the state text a caller sent is
/// never part of this struct (see [`ask`]'s doc for where it *is* kept, only
/// when `recordDecisionInputs` is on).
#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub spec: String,
    pub version: u32,
    pub by: DecidedBy,
    pub answer: String,
    pub confidence: f64,
    pub band: &'static str,
    pub latency_ms: f64,
    /// Logged, never acted on — the spec was in shadow (or the global
    /// `controlPlane.shadow` opt-in was what made the call happen at all).
    pub shadow: bool,
}

/// Whether a spec's Jev tier should be asked at all this call. `route.skill`
/// / `tool.risk` never come through here — their existing call sites (the
/// gate, the router) keep deciding for themselves; this is for the shadow
/// specs that have no other code path.
pub fn should_ask(spec: &Spec, settings: &ControlPlaneSettings) -> bool {
    if super::jev_off(settings) {
        return false;
    }
    match spec.mode {
        SpecMode::Off => false,
        SpecMode::Active => true,
        SpecMode::Shadow => settings.shadow,
    }
}

fn text(s: impl Into<String>) -> Json {
    Json::String(s.into())
}

fn object(fields: Vec<(&str, Json)>) -> Json {
    Json::Object(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Build the `questions` half of the wire request from a spec's own
/// definition — the noul/choice/score shape `decision::types::Question::parse`
/// expects. `pub` so a caller that also wants `trace::record_decision_input`
/// (the G1 opt-in) can build the exact same value without a second copy of
/// this logic.
pub fn question_json(spec: &Spec) -> Json {
    let mut fields = vec![("type", text(spec.qtype_str())), ("instructions", text(spec.question.clone()))];
    match (&spec.options, &spec.levels) {
        (Some(opts), _) => fields.push(("criteria", opts.clone())),
        (None, Some(levels)) => fields.push(("criteria", levels.clone())),
        (None, None) => {}
    }
    object(fields)
}

/// Read one answer + its confidence back out of the generic JSON the wire
/// response was converted to — the same trick `decision::gate`/`skill_route`
/// already use to read local and online answers with one code path.
fn extract_answer(qtype_str: &str, answer: &serde_json::Value) -> Option<(String, f64)> {
    match qtype_str {
        "choice" => {
            let choice = answer.get("choice")?.as_str()?.to_string();
            let conf = answer.get("answer_confidence").and_then(|v| v.as_f64())?;
            Some((choice, conf))
        }
        "score" => {
            let score = answer.get("score")?.as_f64()?;
            let conf = answer.get("answer_confidence").and_then(|v| v.as_f64())?;
            Some((format!("{score}"), conf))
        }
        "noul" => {
            let noul = answer.get("noul")?.as_f64()?;
            let conf = answer.get("confidence").and_then(|v| v.as_f64())?;
            Some((if noul >= 0.5 { "true".to_string() } else { "false".to_string() }, conf))
        }
        _ => None,
    }
}

/// The exact `questions` value a call to [`ask`] would send — exposed so a
/// caller recording the G1 opt-in (`trace::record_decision_input`) records
/// the real wire value, not a second hand-built copy of it.
pub fn build_request_questions(spec: &Spec) -> Json {
    object(vec![("q", question_json(spec))])
}

/// Ask one spec. Returns `None` when it was not asked at all (off, JEV_OFF,
/// shadow opt-out) — the caller then has literally nothing to do, which is
/// the point: every new spec degrades to "as if this module did not exist".
///
/// `state` is exactly what reaches the decision runtime — callers that also
/// want it recorded for the G1 prefix regression do so themselves through
/// `trace::record_decision_input`, gated on `recordDecisionInputs`, never
/// bundled into this call so a state leak cannot happen by only forgetting
/// one flag check.
pub async fn ask(
    runtime: &RuntimeManager,
    settings: &ControlPlaneSettings,
    spec: &Spec,
    state: Json,
) -> Option<Decision> {
    if !should_ask(spec, settings) {
        return None;
    }
    let request = AskRequest {
        backend: None,
        model: None,
        state,
        questions: build_request_questions(spec),
    };
    let started = Instant::now();
    let answered = tokio::time::timeout(SPEC_TIMEOUT, client::ask(runtime, &request)).await;
    let latency_ms = (started.elapsed().as_secs_f64() * 10_000.0).round() / 10.0;
    let (answer, confidence) = match answered {
        Ok(Ok(resp)) => {
            let answers = serde_json::to_value(&resp.answers).unwrap_or_default();
            match extract_answer(spec.qtype_str(), &answers["q"]) {
                Some(pair) => pair,
                None => {
                    tracing::debug!("[control-plane] {}: unreadable answer shape", spec.id);
                    return Some(unreadable(spec, latency_ms));
                }
            }
        }
        Ok(Err(e)) => {
            tracing::debug!("[control-plane] {}: decision runtime error: {e}", spec.id);
            return Some(unreadable(spec, latency_ms));
        }
        Err(_) => {
            tracing::debug!("[control-plane] {}: timed out after {SPEC_TIMEOUT:?}", spec.id);
            return Some(unreadable(spec, latency_ms));
        }
    };
    let band = spec.bands.classify(confidence);
    Some(Decision {
        spec: spec.id.clone(),
        version: spec.version,
        by: DecidedBy::Jev,
        answer,
        confidence,
        band: band.as_str(),
        latency_ms,
        shadow: spec.mode != SpecMode::Active,
    })
}

/// G1 prefix regression only: ask a spec regardless of its `mode` (still
/// honouring `JEV_OFF`, the one switch that must stay absolute). Recorded
/// decision inputs are replayed against the *current* registry to check the
/// new answer still falls in `criteria.decision_assertions`'s acceptable set
/// — that check is meaningless if a spec sitting in `shadow` or `off` is
/// silently skipped, so this bypasses [`should_ask`]'s mode gate on purpose.
/// Never call this from a live turn.
pub async fn force_ask(runtime: &RuntimeManager, settings: &ControlPlaneSettings, spec: &Spec, state: Json) -> Option<Decision> {
    if super::jev_off(settings) {
        return None;
    }
    let request = AskRequest { backend: None, model: None, state, questions: build_request_questions(spec) };
    let started = Instant::now();
    let answered = tokio::time::timeout(SPEC_TIMEOUT, client::ask(runtime, &request)).await;
    let latency_ms = (started.elapsed().as_secs_f64() * 10_000.0).round() / 10.0;
    let (answer, confidence) = match answered {
        Ok(Ok(resp)) => {
            let answers = serde_json::to_value(&resp.answers).unwrap_or_default();
            extract_answer(spec.qtype_str(), &answers["q"]).unwrap_or_else(|| (String::new(), 0.0))
        }
        _ => return Some(unreadable(spec, latency_ms)),
    };
    let band = spec.bands.classify(confidence);
    Some(Decision {
        spec: spec.id.clone(),
        version: spec.version,
        by: DecidedBy::Jev,
        answer,
        confidence,
        band: band.as_str(),
        latency_ms,
        shadow: true, // a replay never acts regardless of the spec's live mode
    })
}

/// A timeout or an engine error is not "no decision" — it is worth exactly as
/// much to the trace as a low-confidence answer: `on_uncertain`'s behaviour
/// applies (nothing acts on it), so it is banded `review` at zero confidence
/// rather than silently dropped.
fn unreadable(spec: &Spec, latency_ms: f64) -> Decision {
    Decision {
        spec: spec.id.clone(),
        version: spec.version,
        by: DecidedBy::Jev,
        answer: String::new(),
        confidence: 0.0,
        band: Band::Review.as_str(),
        latency_ms,
        shadow: spec.mode != SpecMode::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::super::registry::{Bands, Lifecycle, SpecType};
    use super::*;

    fn spec(mode: SpecMode) -> Spec {
        Spec {
            id: "clarify.needed".into(),
            version: 1,
            qtype: SpecType::Noul,
            question: "does it hold?".into(),
            options: None,
            levels: None,
            state_fields: vec!["query".into()],
            bands: Bands { act: 0.8, fallback: 0.5 },
            on_uncertain: "proceed".into(),
            mode,
            lifecycle: Lifecycle::Shadow,
            lang: "en".into(),
            wraps_existing: false,
        }
    }

    fn empty_runtime(tmp: &std::path::Path) -> std::sync::Arc<RuntimeManager> {
        RuntimeManager::new(crate::runtime::manager::RuntimeManagerConfig {
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
    fn should_ask_follows_the_leads_opt_in_rule() {
        let mut settings = ControlPlaneSettings::default();
        assert!(!should_ask(&spec(SpecMode::Off), &settings));
        assert!(!should_ask(&spec(SpecMode::Shadow), &settings), "shadow needs the global opt-in");
        assert!(should_ask(&spec(SpecMode::Active), &settings), "active always asks");

        settings.shadow = true;
        assert!(should_ask(&spec(SpecMode::Shadow), &settings));

        settings.jev_off = true;
        assert!(!should_ask(&spec(SpecMode::Active), &settings), "JEV_OFF beats even active");
    }

    #[tokio::test]
    async fn a_spec_that_should_not_be_asked_never_touches_the_network() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = empty_runtime(tmp.path());
        let settings = ControlPlaneSettings::default(); // shadow off, spec is shadow
        let d = ask(&runtime, &settings, &spec(SpecMode::Shadow), Json::String("x".into())).await;
        assert!(d.is_none());
    }

    #[tokio::test]
    async fn an_unavailable_engine_still_returns_a_reviewable_decision() {
        let tmp = tempfile::tempdir().unwrap(); // no decision runtime installed
        let runtime = empty_runtime(tmp.path());
        let mut settings = ControlPlaneSettings::default();
        settings.shadow = true;
        let d = ask(&runtime, &settings, &spec(SpecMode::Shadow), Json::String("x".into()))
            .await
            .unwrap();
        assert_eq!(d.band, "review");
        assert_eq!(d.confidence, 0.0);
        assert!(d.shadow);
    }

    #[tokio::test]
    async fn force_ask_bypasses_the_mode_gate_but_not_jev_off() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = empty_runtime(tmp.path());
        let settings = ControlPlaneSettings::default(); // shadow off
        // An `off` spec would never be asked through `ask`, but `force_ask`
        // (used only for G1 replay) still tries — no engine here, so it comes
        // back as a reviewable "unreadable" decision rather than `None`.
        let d = force_ask(&runtime, &settings, &spec(SpecMode::Off), Json::String("x".into())).await;
        assert!(d.is_some(), "force_ask must not honor the off mode");

        let mut jev_off_settings = settings.clone();
        jev_off_settings.jev_off = true;
        let d = force_ask(&runtime, &jev_off_settings, &spec(SpecMode::Active), Json::String("x".into())).await;
        assert!(d.is_none(), "JEV_OFF still wins even for a forced replay");
    }

    #[test]
    fn extract_answer_reads_each_question_type() {
        assert_eq!(
            extract_answer("noul", &serde_json::json!({"noul": 0.9, "confidence": 0.9})),
            Some(("true".into(), 0.9))
        );
        assert_eq!(
            extract_answer("choice", &serde_json::json!({"choice": "a", "answer_confidence": 0.7})),
            Some(("a".into(), 0.7))
        );
        assert_eq!(
            extract_answer("score", &serde_json::json!({"score": 3.0, "answer_confidence": 0.6})),
            Some(("3".into(), 0.6))
        );
        assert_eq!(extract_answer("noul", &serde_json::json!({})), None);
    }
}

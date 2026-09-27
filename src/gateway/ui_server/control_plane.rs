//! Control-plane registry + trace endpoints (P1):
//!
//!   GET  /api/control-plane/specs           — every spec, `route.skill`/`tool.risk`'s
//!                                              `mode` live-synced from `decisionConfig`
//!   PUT  /api/control-plane/specs/:id/mode  — `{ mode }`; refused for a wrapping spec
//!   GET  /api/traces                        — recent traces across all chats
//!   GET  /api/traces/:id                    — one trace, full body
//!   POST /api/control-plane/decisions/replay — G1 prefix regression: force-ask
//!                                              a spec against a recorded state
//!
//! The logic lives in [`crate::control_plane`]; this file maps it onto HTTP.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::control_plane::registry::{SpecMode, SpecRegistry};
use crate::control_plane::{ladder, ControlPlaneSettings};
use crate::decision::settings::FeatureMode;
use crate::gateway::group_manager::load_decision_settings;

use super::core::{AppError, UiState};

fn bad(msg: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.into())
}

fn feature_mode_to_spec_mode(m: FeatureMode) -> &'static str {
    match m {
        FeatureMode::Off => "off",
        FeatureMode::Shadow => "shadow",
        FeatureMode::On => "active",
    }
}

/// A wrapping spec's `mode` field is descriptive, not authoritative — this
/// overlays the live `decisionConfig` value so the response never lies about
/// what is actually driving the pre-skill router / tool-call gate.
fn live_synced_view(s: &UiState, reg: &SpecRegistry) -> Vec<Value> {
    let decision_settings = load_decision_settings(&s.config.paths.global_config_path);
    reg.list()
        .into_iter()
        .map(|spec| {
            let mut v = serde_json::to_value(spec).unwrap_or_default();
            if spec.wraps_existing {
                let live = match spec.id.as_str() {
                    "route.skill" => Some(feature_mode_to_spec_mode(decision_settings.skills.mode)),
                    "tool.risk" => Some(feature_mode_to_spec_mode(decision_settings.gate.mode)),
                    _ => None,
                };
                if let Some(live) = live {
                    v["mode"] = json!(live);
                }
            }
            v
        })
        .collect()
}

pub(crate) async fn specs_list(State(s): State<Arc<UiState>>) -> Result<Json<Value>, AppError> {
    let reg = SpecRegistry::load();
    Ok(Json(json!({
        "specs": live_synced_view(&s, &reg),
        "errors": reg.errors,
    })))
}

#[derive(Deserialize)]
pub(crate) struct SpecModeBody {
    mode: SpecMode,
}

pub(crate) async fn spec_mode_put(
    State(s): State<Arc<UiState>>,
    Path(id): Path<String>,
    Json(body): Json<SpecModeBody>,
) -> Result<Json<Value>, AppError> {
    let mut reg = SpecRegistry::load();
    reg.set_mode(&id, body.mode).map_err(bad)?;
    Ok(Json(json!({ "ok": true, "specs": live_synced_view(&s, &reg) })))
}

#[derive(Deserialize)]
pub(crate) struct TracesQuery {
    #[serde(default)]
    limit: Option<usize>,
}

pub(crate) async fn traces_list(Query(q): Query<TracesQuery>) -> Result<Json<Value>, AppError> {
    let limit = q.limit.unwrap_or(50).min(500);
    Ok(Json(json!({ "traces": crate::control_plane::trace::list_all(limit) })))
}

pub(crate) async fn trace_get(Path(id): Path<String>) -> Result<Json<Value>, AppError> {
    match crate::control_plane::trace::read_by_id(&id) {
        Some(v) => Ok(Json(v)),
        None => Err(AppError(StatusCode::NOT_FOUND, format!("no trace {id:?}"))),
    }
}

#[derive(Deserialize)]
pub(crate) struct ReplayBody {
    spec_id: String,
    /// Whatever `state` was recorded for this spec by
    /// `controlPlane.recordDecisionInputs` — `scripts/evals/run.py`'s G1
    /// replay reads it straight from that file and posts it back unchanged.
    state: crate::decision::json::Json,
}

/// G1 prefix regression's one live seam: given a recorded `state`, ask the
/// *current* spec definition again, ignoring its mode (a `shadow` or `off`
/// spec must still be checkable against `criteria.decision_assertions`).
pub(crate) async fn decisions_replay(
    State(s): State<Arc<UiState>>,
    Json(body): Json<ReplayBody>,
) -> Result<Json<Value>, AppError> {
    let reg = SpecRegistry::load();
    let spec = reg
        .get(&body.spec_id)
        .ok_or_else(|| bad(format!("no spec {:?} in the registry", body.spec_id)))?;
    let Some(runtime) = s.runtime_manager.as_ref() else {
        return Err(AppError(StatusCode::SERVICE_UNAVAILABLE, "the runtime manager is not wired".into()));
    };
    let settings: ControlPlaneSettings =
        crate::gateway::group_manager::load_control_plane_settings(&s.config.paths.global_config_path);
    let decision = ladder::force_ask(runtime, &settings, spec, body.state).await;
    Ok(Json(json!({ "decision": decision })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_mode_maps_onto_the_three_spec_modes() {
        assert_eq!(feature_mode_to_spec_mode(FeatureMode::Off), "off");
        assert_eq!(feature_mode_to_spec_mode(FeatureMode::Shadow), "shadow");
        assert_eq!(feature_mode_to_spec_mode(FeatureMode::On), "active");
    }
}

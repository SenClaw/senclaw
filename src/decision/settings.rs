//! What the daemon itself still owns of `decisionConfig`: the tool-call gate
//! and the pre-turn skill router. `backend`/`local`/`online` moved to the
//! `sen-sysone` runtime, which owns them as its own opaque sub-keys of the
//! same JSON object — the daemon reads and writes only `.gate`/`.skills` and
//! must never touch the rest (`docs/runtime-protocol.md` §8).

use serde::{Deserialize, Serialize};

/// How far a decision feature (the tool-call gate, the pre-skill router)
/// is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FeatureMode {
    /// Not asked at all; the old behaviour.
    #[default]
    Off,
    /// Asked and recorded, but the old behaviour still applies — how to
    /// measure the feature on real traffic before trusting it.
    Shadow,
    /// The feature's answer applies.
    On,
}

/// Which questions the gate asks. `Auto` used to follow the local/online
/// backend choice; the daemon no longer tracks that (it lives in sen-sysone
/// now), so `Auto` resolves to `Laya` — the shape the default local backend
/// understands, and the safer of the two to ask blind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GateQuestions {
    #[default]
    Auto,
    /// One choice — what the command does — which Laya classifies well.
    Laya,
    /// The cookbook's `reversible` noul, verbatim; Jev answers it, zero-shot
    /// Laya does not (every read-only command scores under 0.1).
    Cookbook,
}

pub const DEFAULT_GATE_APPROVE_AT: f64 = 0.9;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateSettings {
    #[serde(default)]
    pub mode: FeatureMode,
    /// Approve only at or above this probability that the command just reads,
    /// tests or edits inside the project.
    #[serde(default = "default_approve_at")]
    pub approve_at: f64,
    #[serde(default)]
    pub questions: GateQuestions,
}

impl Default for GateSettings {
    fn default() -> Self {
        GateSettings {
            mode: FeatureMode::Off,
            approve_at: DEFAULT_GATE_APPROVE_AT,
            questions: GateQuestions::Auto,
        }
    }
}

fn default_approve_at() -> f64 {
    DEFAULT_GATE_APPROVE_AT
}

/// The pre-turn skill router (`crate::decision::skill_route`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SkillRouteSettings {
    #[serde(default)]
    pub mode: FeatureMode,
}

/// The daemon-owned slice of `decisionConfig`. Constructed from (and merged
/// back into) the raw JSON object via [`DecisionSettings::from_raw`] /
/// [`DecisionSettings::merge_into_raw`] so `backend`/`local`/`online` —
/// sen-sysone's sub-keys — round-trip untouched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DecisionSettings {
    #[serde(default)]
    pub gate: GateSettings,
    #[serde(default)]
    pub skills: SkillRouteSettings,
}

impl DecisionSettings {
    /// Read `.gate`/`.skills` out of the raw `decisionConfig` value, leniently
    /// — an unreadable sub-key costs only itself, never the whole daemon.
    pub fn from_raw(raw: &serde_json::Value) -> DecisionSettings {
        let gate = raw
            .get("gate")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let skills = raw
            .get("skills")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        DecisionSettings { gate, skills }
    }

    /// Set `.gate`/`.skills` on `raw`, leaving every other sub-key
    /// (`backend`/`local`/`online`, sen-sysone's) exactly as it was.
    pub fn merge_into_raw(&self, raw: &mut serde_json::Value) {
        if !raw.is_object() {
            *raw = serde_json::json!({});
        }
        let obj = raw.as_object_mut().expect("just ensured this is an object");
        obj.insert("gate".to_string(), serde_json::to_value(&self.gate).unwrap_or_default());
        obj.insert("skills".to_string(), serde_json::to_value(&self.skills).unwrap_or_default());
    }

    /// Clamp what is merely out of range. Below 0.5 "approve" would mean
    /// "more likely unsafe than not".
    pub fn validated(mut self) -> DecisionSettings {
        self.gate.approve_at = if self.gate.approve_at.is_finite() {
            self.gate.approve_at.clamp(0.5, 0.99)
        } else {
            DEFAULT_GATE_APPROVE_AT
        };
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_config_starts_the_gate_and_skills_off() {
        let s = DecisionSettings::from_raw(&serde_json::json!({}));
        assert_eq!(s.gate, GateSettings::default());
        assert_eq!(s.skills, SkillRouteSettings::default());
    }

    #[test]
    fn the_gate_threshold_stays_where_approve_means_likely_safe() {
        let mut s = DecisionSettings::default();
        s.gate.approve_at = 0.2;
        assert_eq!(s.clone().validated().gate.approve_at, 0.5);
        s.gate.approve_at = 1.0;
        assert_eq!(s.clone().validated().gate.approve_at, 0.99);
        s.gate.approve_at = f64::NAN;
        assert_eq!(s.validated().gate.approve_at, DEFAULT_GATE_APPROVE_AT);
        let g: GateSettings = serde_json::from_str(r#"{"mode": "shadow", "questions": "cookbook"}"#).unwrap();
        assert_eq!((g.mode, g.questions, g.approve_at), (FeatureMode::Shadow, GateQuestions::Cookbook, 0.9));
    }

    #[test]
    fn merging_leaves_sen_sysones_sub_keys_untouched() {
        let mut raw = serde_json::json!({
            "backend": "online",
            "local": {"idleUnloadMinutes": 30},
            "online": {"provider": "typesafe", "apiKey": "sk-secret"},
            "gate": {"mode": "on"},
        });
        let mut settings = DecisionSettings::from_raw(&raw);
        assert_eq!(settings.gate.mode, FeatureMode::On);
        settings.skills.mode = FeatureMode::Shadow;
        settings.merge_into_raw(&mut raw);

        assert_eq!(raw["backend"], "online");
        assert_eq!(raw["local"]["idleUnloadMinutes"], 30);
        assert_eq!(raw["online"]["apiKey"], "sk-secret");
        assert_eq!(raw["skills"]["mode"], "shadow");
    }

    #[test]
    fn merging_into_a_non_object_starts_fresh_instead_of_failing() {
        let mut raw = serde_json::Value::Null;
        let settings = DecisionSettings::default();
        settings.merge_into_raw(&mut raw);
        assert!(raw.is_object());
        assert_eq!(raw["gate"]["mode"], "off");
    }
}

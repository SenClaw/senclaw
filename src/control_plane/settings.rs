//! The `controlPlane` block in `config.json`. Fully daemon-owned — unlike
//! `decisionConfig`, no runtime shares this key, so it round-trips as a typed
//! struct instead of a raw `Value` merge.

use serde::{Deserialize, Serialize};

/// Per-session working directory: passive mirror only (progress.md/todo.json
/// mirror what already happened; never changes what a tool returns to the
/// LLM) unless `substituteToolOutput` is explicitly turned on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSettings {
    /// Mirror progress/todo/artifacts to disk. Read-only from the agent's
    /// point of view — never changes a tool result or a decision. Default on
    /// for the same reason trace is: it costs a few small file writes, no
    /// Jev call, no extra RAM.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// L1 offload (§8): replace a large tool result with a head+tail preview
    /// pointing at the full file under `artifacts/<call_id>`. Off by default
    /// — this is the one part of Workspace that changes what the LLM sees,
    /// so it stays behind a switch until an eval shows it does not regress
    /// tasks that need the full output inline.
    #[serde(default)]
    pub substitute_tool_output: bool,
}

impl Default for WorkspaceSettings {
    fn default() -> Self {
        WorkspaceSettings {
            enabled: true,
            substitute_tool_output: false,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlPlaneSettings {
    /// Ablation baseline (§13): skip every Jev tier, including the existing
    /// gate/router engine calls. `crate::control_plane::jev_off` also checks
    /// `SENCLAW_JEV_OFF=1`, which wins over this field.
    #[serde(default)]
    pub jev_off: bool,
    /// Opt-in per the lead's decision: a shadow-mode spec calls the decision
    /// runtime only when this is on, or when that one spec's own `mode` is
    /// `active`. Default off — a default install must not start the
    /// `sen-sysone` process (1.2-1.7 GB RSS for a Laya load) just to collect
    /// labels on every chat turn.
    #[serde(default)]
    pub shadow: bool,
    /// `<agent_status>` tail appended per LLM call (§7), computed by code —
    /// never a decision, never a web-content leak. Off by default all the
    /// same: the block rides on the *last* user-role message, which is a
    /// different message on every call of a multi-step turn, so the previous
    /// call's prompt is never a prefix of the next one. A local engine reuses
    /// its KV cache only on an exact prefix (sen-mlx), so with this on every
    /// agent step re-prefilled the whole prompt — measured on Gemma 4 E2B,
    /// a 16k-token step took ~30s instead of ~1.3s.
    #[serde(default)]
    pub agent_status: bool,
    /// G1 prefix regression (§13): record the exact `state`/`questions` sent
    /// to a spec, not just the metadata trace normally keeps. Off by
    /// default — this is the one record that necessarily holds message-shaped
    /// content, so it is opt-in even though the rest of the trace is not.
    #[serde(default)]
    pub record_decision_inputs: bool,
    #[serde(default)]
    pub workspace: WorkspaceSettings,
}

impl Default for ControlPlaneSettings {
    fn default() -> Self {
        ControlPlaneSettings {
            jev_off: false,
            shadow: false,
            agent_status: false,
            record_decision_inputs: false,
            workspace: WorkspaceSettings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_leads_decision() {
        let s = ControlPlaneSettings::default();
        assert!(!s.jev_off);
        assert!(!s.shadow, "shadow must default off — no RAM cost on a default install");
        assert!(!s.agent_status, "agent_status moves every call and would defeat a local engine's prefix cache");
        assert!(!s.record_decision_inputs);
        assert!(s.workspace.enabled);
        assert!(!s.workspace.substitute_tool_output);
    }

    #[test]
    fn an_absent_block_deserializes_to_defaults() {
        let s: ControlPlaneSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, ControlPlaneSettings::default());
    }

    #[test]
    fn round_trips_camel_case_field_names() {
        let text = serde_json::to_string(&ControlPlaneSettings {
            jev_off: true,
            shadow: true,
            agent_status: false,
            record_decision_inputs: true,
            workspace: WorkspaceSettings { enabled: false, substitute_tool_output: true },
        })
        .unwrap();
        assert!(text.contains("\"jevOff\":true"), "{text}");
        assert!(text.contains("\"recordDecisionInputs\":true"), "{text}");
        assert!(text.contains("\"substituteToolOutput\":true"), "{text}");
        let back: ControlPlaneSettings = serde_json::from_str(&text).unwrap();
        assert!(back.jev_off && back.shadow && !back.agent_status);
    }
}

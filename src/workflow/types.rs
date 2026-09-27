//! Workflow type definitions.
//!
//! Port of `SemaClaw/src/workflow/types.ts`.
//!
//! Two families of objects:
//!   - Definitions (`WorkflowDef` / `WorkflowStep` ...): declarative data from
//!     `<workflows_dir>/<name>.md` YAML frontmatter.
//!   - Runs (`WorkflowRun` / `StepRun` ...): one execution's state, persisted
//!     to `workflow-runs.json`.
//!
//! Run records serialize in camelCase to stay wire-compatible with the
//! upstream TypeScript UI/state format.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ============================================================
// Definitions (declarative, from .md frontmatter)
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    Agent,
    Script,
}

/// Run-level input parameter declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInput {
    pub name: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Observe display tier: inline = markdown on the node / artifact = Workbench viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObserveAs {
    Inline,
    Artifact,
}

/// Observe source: the step's `result`, or a file inside the run workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserveFrom {
    Result,
    File(String),
}

/// Optional human-facing intermediate output (pure observation, not part of
/// the DAG or data flow).
#[derive(Debug, Clone)]
pub struct ObserveSpec {
    pub label: String,
    pub from: ObserveFrom,
    pub r#as: ObserveAs,
}

/// One workflow step (DAG node).
#[derive(Debug, Clone)]
pub struct WorkflowStep {
    pub id: String,
    pub kind: StepKind,
    /// Upstream step ids; empty = entry node.
    pub depends_on: Vec<String>,
    /// Timeout in seconds. Defaults to 600 for both kinds.
    pub timeout: Option<u64>,
    /// Step-level rules/constraints (→ custom_rules, agent steps only).
    pub guidance: Option<String>,
    pub observe: Option<ObserveSpec>,

    // kind: agent
    /// Persona name (→ PersonaRegistry::get).
    pub persona: Option<String>,
    /// Task prompt (→ process_user_input), supports `{{}}` interpolation.
    pub prompt: Option<String>,

    // kind: script
    /// Inline shell command; mutually exclusive with `script_file`.
    pub run: Option<String>,
    /// Script file path (relative to the def dir or absolute).
    pub script_file: Option<String>,
}

/// A workflow definition.
#[derive(Debug, Clone)]
pub struct WorkflowDef {
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub inputs: Vec<WorkflowInput>,
    /// Workflow-level rules applied to all agent steps (joined with step guidance).
    pub guidance: Option<String>,
    /// Custom workspace dir (cwd of every step, persistent across runs).
    /// None = default `<workflow_data_dir>/<sanitized-name>/`.
    pub workspace: Option<String>,
    pub steps: Vec<WorkflowStep>,
    /// Absolute path of the definition file.
    pub file_path: PathBuf,
    /// Source layer: "user" | "project" (MVP: user only).
    pub source: String,
}

// ============================================================
// Runs (state, persisted)
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl StepStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Skipped)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "done")]
    Done,
    #[serde(rename = "partial-failed")]
    PartialFailed,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "interrupted")]
    Interrupted,
}

/// Captured observe output (stored in the run record, pushed to the UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObserveOutput {
    pub label: String,
    pub r#as: ObserveAs,
    /// inline: markdown text; artifact: empty (use `artifact_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// artifact: absolute path of a file inside the run workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
}

/// One step's run state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepRun {
    pub id: String,
    pub kind: StepKind,
    /// Persona snapshot for agent steps (history shows which persona ran).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// Dependency snapshot copied from the def so the record is self-contained.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    pub status: StepStatus,
    /// agent = final message / script = stdout.
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observe: Option<ObserveOutput>,
    /// Rendered guidance snapshot (for history).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance_snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

/// One workflow run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRun {
    pub id: String,
    pub workflow_name: String,
    /// Optional user-given display name (rename in the UI). Falls back to
    /// the run id when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub inputs: HashMap<String, String>,
    pub status: RunStatus,
    /// Shared workspace dir of this run (persistent per workflow).
    pub run_dir: String,
    pub steps: Vec<StepRun>,
    /// Trigger source: cli / schedule / ui.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

// ============================================================
// Execution-time context (passed to template / step runners)
// ============================================================

/// Data visible when rendering `{{}}`: inputs + completed step results.
#[derive(Debug, Clone, Default)]
pub struct RenderContext {
    pub inputs: HashMap<String, String>,
    /// step id → result (completed steps only).
    pub step_results: HashMap<String, String>,
    pub run_dir: String,
}

/// Sanitize a workflow name / step id into a safe file-name segment.
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_status_serializes_like_upstream() {
        assert_eq!(
            serde_json::to_string(&RunStatus::PartialFailed).unwrap(),
            "\"partial-failed\""
        );
        assert_eq!(
            serde_json::to_string(&StepStatus::Skipped).unwrap(),
            "\"skipped\""
        );
    }

    #[test]
    fn step_run_camel_case_fields() {
        let sr = StepRun {
            id: "a".into(),
            kind: StepKind::Agent,
            persona: Some("p".into()),
            depends_on: vec!["b".into()],
            status: StepStatus::Done,
            result: "r".into(),
            error: None,
            observe: None,
            guidance_snapshot: Some("g".into()),
            started_at: None,
            completed_at: None,
        };
        let json = serde_json::to_string(&sr).unwrap();
        assert!(json.contains("\"dependsOn\""));
        assert!(json.contains("\"guidanceSnapshot\""));
    }

    #[test]
    fn sanitize_name_replaces_specials() {
        assert_eq!(sanitize_name("hello world/x"), "hello_world_x");
        assert_eq!(sanitize_name("a.b_c-d"), "a.b_c-d");
    }
}

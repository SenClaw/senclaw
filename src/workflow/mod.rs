//! Workflow — saved, parameterized DAGs of agent + script steps.
//!
//! Port of upstream `SemaClaw/src/workflow/` (TypeScript). A workflow is a
//! Markdown file with YAML frontmatter in `<workflows_dir>`; each step is
//! either an `agent` step (isolated one-shot persona session) or a `script`
//! step (shell). The executor is fully decoupled from AgentPool /
//! DispatchBridge — it only spawns isolated sessions and child processes.

pub mod executor;
pub mod registry;
pub mod run_store;
pub mod service;
pub mod settings;
pub mod step_runners;
pub mod template;
pub mod types;

pub use executor::{WorkflowExecutor, WorkflowExecutorOpts};
pub use registry::{parse_workflow_file, WorkflowRegistry};
pub use run_store::WorkflowRunStore;
pub use service::{WorkflowService, WorkflowServiceOpts};
pub use settings::{LiveWorkflowSettings, WorkflowSettings};
pub use types::{RunStatus, StepStatus, WorkflowDef, WorkflowRun};

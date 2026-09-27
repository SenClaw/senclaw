//! EnterPlanMode tool — explicitly switch the agent into Plan mode.
//!
//! In Plan mode, write tools should be limited to the plan file. The model
//! is expected to compose a plan, then call `ExitPlanMode` for approval, and
//! only execute after the user signs off.
//!
//! Engine-side this flips `ZenCoreOptions::agent_mode = Plan`, which has
//! these knock-on effects via `tools_for_main_agent`:
//!   - `TodoWrite` is dropped from the active tool list (Plan-mode policy)
//!   - The plan-mode reminder block is injected on the next turn
//!
//! Pair with `ExitPlanMode` (already shipped) to complete the loop.

use std::sync::Weak;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{
    AgentMode, Tool, ToolContext, ToolOutput, ToolResultMessage, ZenCore, ZenEngine,
};

/// Callback that switches the engine into Plan mode. Avoids a hard
/// dependency on `ZenEngine` from the tool API surface.
pub type EnterPlanFn = std::sync::Arc<dyn Fn() + Send + Sync>;

pub struct EnterPlanModeTool {
    enter: EnterPlanFn,
}

impl EnterPlanModeTool {
    pub fn new(enter: EnterPlanFn) -> Self {
        Self { enter }
    }

    /// Build a tool tied to the given engine. The closure flips the engine's
    /// `agent_mode` to `Plan` on call.
    pub fn for_engine(engine: Weak<ZenEngine>) -> Self {
        Self::new(std::sync::Arc::new(move || {
            if let Some(e) = engine.upgrade() {
                e.update_agent_mode(AgentMode::Plan);
                tracing::info!("[EnterPlanMode] agent_mode → Plan");
            }
        }))
    }
}

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn description(&self) -> &str {
        "Switch the session into Plan mode. While in Plan mode, you must NOT \
         edit source files — only write the plan markdown to the workspace's \
         plans/ directory. End the plan by calling `ExitPlanMode` to request \
         user approval. Use Plan mode for new project scaffolding, multi-file \
         refactors, or any task spanning 3+ files."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "One-line rationale shown in logs (e.g. 'multi-file React scaffold')."
                }
            }
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    /// Always include in the active tool list — discovery for Plan mode
    /// shouldn't require ToolSearch round-trip.
    fn always_load(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        (self.enter)();
        let text = if reason.is_empty() {
            "Switched to Plan mode. Next steps: (1) write the plan to <workspace>/plans/<name>.md, (2) call ExitPlanMode { plan, planFilePath } for user approval, (3) do NOT touch source files until approved.".to_string()
        } else {
            format!(
                "Switched to Plan mode — reason: {reason}. Next steps: (1) write the plan to <workspace>/plans/<name>.md, (2) call ExitPlanMode {{ plan, planFilePath }} for user approval, (3) do NOT touch source files until approved."
            )
        };
        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({"mode": "Plan", "reason": reason}),
            result_for_assistant: text,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let reason = data
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        ToolResultMessage {
            title: "EnterPlanMode".to_string(),
            summary: if reason.is_empty() {
                "Plan mode enabled".to_string()
            } else {
                format!("Plan mode: {reason}")
            },
            content: data.clone(),
        }
    }

    fn get_display_title(&self, _input: &Value) -> String {
        "EnterPlanMode".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn call_flips_via_closure() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_cb = Arc::clone(&flag);
        let tool = EnterPlanModeTool::new(Arc::new(move || {
            flag_for_cb.store(true, Ordering::SeqCst);
        }));
        let ctx = ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let out = tool
            .call(serde_json::json!({"reason": "test"}), &ctx)
            .await
            .unwrap();
        assert!(flag.load(Ordering::SeqCst));
        let ToolOutput::Result { data, .. } = &out[0] else {
            panic!();
        };
        assert_eq!(data["mode"], "Plan");
    }

    #[test]
    fn enter_plan_mode_is_always_loaded() {
        let tool = EnterPlanModeTool::new(Arc::new(|| {}));
        assert!(tool.always_load());
    }
}

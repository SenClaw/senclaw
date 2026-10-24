//! ExitPlanMode tool — the agent calls this to request user approval of a plan,
//! mirroring sema-core's `plan_to_agent` tool.
//!
//! Behaviour: emits a [`crate::zen_core::EngineEvent::PlanExitRequest`] event
//! with the plan file path and content; suspends until the user picks
//! `startEditing` or `clearContextAndStart`. The chosen option is delivered
//! via the engine's response registry.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{
    EngineEvent, PlanExitOptions, PlanExitRequestData, Tool, ToolContext, ToolOutput,
    ToolPermissionInfo, ToolResultMessage,
};

pub struct ExitPlanModeTool;

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &str {
        "ExitPlanMode"
    }

    fn description(&self) -> &str {
        "Use this tool when you are in plan mode and have finished presenting your plan and are \
         ready to code. Prompts the user to approve the plan."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "Markdown plan content to present to the user for approval."
                },
                "planFilePath": {
                    "type": "string",
                    "description": "Absolute path of the plan file you wrote. Optional but recommended."
                }
            },
            "required": ["plan"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let plan_content = input
            .get("plan")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let plan_file_path = input
            .get("planFilePath")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // The plan is what the person reads and approves, and what a restarted run is carried
        // out from. An empty one opened an approval card reading "No plan content" that a click
        // approved, after which the run had nothing to do. Refuse it here, before anything is
        // shown, and tell the model where the plan goes: a model that wrote its plan as an
        // ordinary reply and then called this with `plan` empty is the usual way to get here.
        if plan_content.trim().is_empty() {
            tracing::warn!("[ExitPlanMode] agent={} called with an empty plan - refused", ctx.agent_id);
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"error": "empty_plan"}),
                result_for_assistant: "Error: ExitPlanMode was called with an empty `plan`, so nothing was shown to the person. \
                    The plan is what they read and approve and what the work is carried out from: put the whole plan, in Markdown, \
                    in the `plan` argument (writing it as a normal reply does not count) and call ExitPlanMode again."
                    .to_string(),
            }]);
        }

        tracing::info!(
            "[ExitPlanMode] agent={} plan_file={} plan_len={}",
            ctx.agent_id,
            if plan_file_path.is_empty() {
                "(none)"
            } else {
                &plan_file_path
            },
            plan_content.len()
        );

        let (bus, registry) = match (ctx.event_bus, ctx.response_registry) {
            (Some(b), Some(r)) => (b, r),
            _ => {
                tracing::warn!("[ExitPlanMode] no event bus — approval unavailable");
                return Ok(vec![ToolOutput::Result {
                    data: serde_json::json!({"error": "no_event_bus"}),
                    result_for_assistant: "Plan-mode approval is unavailable in this context."
                        .to_string(),
                }]);
            }
        };

        let rx = registry.register_ask_question(ctx.agent_id);
        tracing::info!("[ExitPlanMode] emitting plan:exit:request, waiting for user approval");
        bus.emit(EngineEvent::PlanExitRequest(PlanExitRequestData {
            agent_id: ctx.agent_id.to_string(),
            plan_file_path: plan_file_path.clone(),
            plan_content: plan_content.clone(),
            options: PlanExitOptions {
                start_editing: "Approve plan and start editing".to_string(),
                clear_context_and_start: "Clear context and start fresh".to_string(),
            },
        }));

        let selected = tokio::select! {
            _ = ctx.abort.cancelled() => "cancelled".to_string(),
            resp = rx => match resp {
                Ok(answer) => {
                    answer.answers
                        .get("selected")
                        .cloned()
                        .unwrap_or_else(|| "startEditing".to_string())
                }
                Err(_) => "cancelled".to_string(),
            }
        };

        tracing::info!(
            "[ExitPlanMode] user selected={selected} agent={}",
            ctx.agent_id
        );

        if selected.as_str() == "clearContextAndStart" {
            bus.emit(EngineEvent::PlanImplement(
                crate::zen_core::PlanImplementData {
                    plan_file_path: plan_file_path.clone(),
                    plan_content: plan_content.clone(),
                },
            ));

            Ok(vec![ToolOutput::ClearContextAndStart {
                plan_file_path,
                plan_content,
            }])
        } else {
            let result_text = match selected.as_str() {
                "cancelled" => "Plan approval was cancelled.".to_string(),
                _ => format!("The plan has been confirmed. Begin implementation whenever ready — update your task list first if applicable.\n\nPlan saved at: {}\n\n## Approved Plan:\n{}\n\n## Plan Mode Exited\n\nEditing, tool use, and all other actions are now available. The plan file remains at {} for reference.", plan_file_path, plan_content, plan_file_path),
            };

            Ok(vec![ToolOutput::Result {
                data: serde_json::json!({
                    "selected": selected,
                    "planFilePath": plan_file_path,
                }),
                result_for_assistant: result_text,
            }])
        }
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let selected = data
            .get("selected")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        ToolResultMessage {
            title: "ExitPlanMode".to_string(),
            summary: selected,
            content: data.clone(),
        }
    }

    fn get_display_title(&self, _input: &Value) -> String {
        "ExitPlanMode".to_string()
    }

    fn gen_tool_permission(&self, _input: &Value) -> Option<ToolPermissionInfo> {
        None // approval flow is handled by the plan-exit event, not the permission gate.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::{EventBus, ResponseRegistry};

    fn ctx<'a>(bus: Option<&'a EventBus>, registry: Option<&'a ResponseRegistry>) -> ToolContext<'a> {
        ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: bus,
            response_registry: registry,
            edit_format: Default::default(),
        }
    }

    #[tokio::test]
    async fn an_empty_plan_is_refused_without_asking_the_person() {
        let bus = EventBus::new();
        let registry = ResponseRegistry::new();
        let mut rx = bus.subscribe();
        for plan in ["", "  \n\t "] {
            let out = ExitPlanModeTool
                .call(serde_json::json!({ "plan": plan }), &ctx(Some(&bus), Some(&registry)))
                .await
                .unwrap();
            let ToolOutput::Result { data, result_for_assistant } = &out[0] else { panic!("a refusal is a plain result") };
            assert_eq!(data["error"], "empty_plan");
            assert!(result_for_assistant.starts_with("Error:"), "the loop reads a failed call by this prefix: {result_for_assistant}");
            assert!(crate::zen_core::finish_check::result_failed("ExitPlanMode", result_for_assistant, false));
            assert!(result_for_assistant.contains("`plan` argument"), "it must say where the plan goes: {result_for_assistant}");
        }
        assert!(rx.try_recv().is_err(), "no approval card for a plan nobody can read");
    }

    #[tokio::test]
    async fn a_missing_plan_argument_is_refused_too() {
        let out = ExitPlanModeTool.call(serde_json::json!({ "planFilePath": "/tmp/p.md" }), &ctx(None, None)).await.unwrap();
        let ToolOutput::Result { data, .. } = &out[0] else { panic!() };
        assert_eq!(data["error"], "empty_plan");
    }

    #[tokio::test]
    async fn a_real_plan_without_an_event_bus_still_reports_no_approval_channel() {
        let out = ExitPlanModeTool.call(serde_json::json!({ "plan": "1. Do the thing" }), &ctx(None, None)).await.unwrap();
        let ToolOutput::Result { data, .. } = &out[0] else { panic!() };
        assert_eq!(data["error"], "no_event_bus");
    }
}

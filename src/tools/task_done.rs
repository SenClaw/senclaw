//! TaskDone tool — explicit "submit" signal for ReAct-style completion.
//!
//! When `conversation::query` sees this tool called, it treats the user's
//! task as finished and exits the loop. Without it, a weak model that emits
//! a text-only summary after a tool call would terminate the conversation
//! prematurely — the engine has no way to distinguish "intermediate status
//! update" from "final answer".
//!
//! Inspired by Inspect AI's `submit()` and Hermes /goal completion pattern.
//! See `senclaw::zen_core::conversation` for the loop integration.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

/// Canonical tool name. The conversation loop matches this verbatim to detect
/// completion, so don't rename without updating `conversation.rs` too.
pub const TASK_DONE_TOOL_NAME: &str = "task_done";

pub struct TaskDoneTool;

#[async_trait]
impl Tool for TaskDoneTool {
    fn name(&self) -> &str {
        TASK_DONE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Signal that the user's task is fully complete. Pass a brief `summary` \
         (1-3 sentences in the user's language) describing what was done and \
         any deliverable produced (file path, URL, key numbers). After calling \
         this, the assistant turn ends and the summary is shown to the user. \
         Only call this when ALL requested steps are finished — including any \
         file writes, reports, or follow-up actions the user asked for. If \
         work remains, call the next tool instead."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Brief summary of completed work in the user's language."
                }
            },
            "required": ["summary"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        let summary = input
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if summary.is_empty() {
            return Err("`summary` is required and must be non-empty".into());
        }
        Ok(())
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let summary = input
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        // The conversation loop short-circuits after this tool runs, so the
        // returned text is mostly for the assistant transcript / event log.
        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({ "summary": summary, "completed": true }),
            result_for_assistant: format!("Task marked complete. Summary: {summary}"),
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "TaskDone".into(),
            summary: data
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(80)
                .collect(),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, _input: &Value) -> String {
        "Task complete".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>() -> ToolContext<'a> {
        ToolContext {
            agent_id: "test",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        }
    }

    #[tokio::test]
    async fn task_done_returns_summary() {
        let tool = TaskDoneTool;
        let out = tool
            .call(serde_json::json!({ "summary": "Đã lưu báo cáo" }), &ctx())
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        match &out[0] {
            ToolOutput::Result {
                data,
                result_for_assistant,
            } => {
                assert_eq!(data["summary"].as_str().unwrap(), "Đã lưu báo cáo");
                assert_eq!(data["completed"].as_bool().unwrap(), true);
                assert!(result_for_assistant.contains("Đã lưu báo cáo"));
            }
            _ => panic!("expected Result"),
        }
    }

    #[tokio::test]
    async fn task_done_rejects_empty_summary() {
        let tool = TaskDoneTool;
        let err = tool
            .validate_input(&serde_json::json!({ "summary": "   " }), &ctx())
            .await
            .unwrap_err();
        assert!(err.contains("required"));
    }

    #[test]
    fn name_is_stable() {
        // The loop in conversation.rs matches this verbatim — guard against rename.
        assert_eq!(TaskDoneTool.name(), "task_done");
        assert_eq!(TASK_DONE_TOOL_NAME, "task_done");
    }
}

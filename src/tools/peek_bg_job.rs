//! PeekBgJob tool — retrieve output of a background job started by Bash.
//!
//! Equivalent of sema-core's `peek_bg_job`. Two modes:
//!   - `wait=true` (default): blocks until the job finishes or `wait_timeout` ms.
//!   - `wait=false`: returns the current snapshot immediately.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::bg_jobs::{BgJobManager, JobStatus};
use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;
const MAX_OUTPUT_RETURN: usize = 30_000;

pub struct PeekBgJobTool;

#[async_trait]
impl Tool for PeekBgJobTool {
    fn name(&self) -> &str {
        "PeekBgJob"
    }

    fn description(&self) -> &str {
        "Retrieve the output of a background job started by Bash with background=true. \
         Use wait=true (default) to wait for completion, or wait=false to get the current snapshot."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": {
                    "type": "string",
                    "description": "ID of the background job to peek at."
                },
                "wait": {
                    "type": "boolean",
                    "description": "When true (default) wait for completion; when false return snapshot.",
                    "default": true
                },
                "wait_timeout": {
                    "type": "number",
                    "description": "Wait timeout in milliseconds (default 30000).",
                    "default": 30000
                }
            },
            "required": ["job_id"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let job_id = input
            .get("job_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let wait = input.get("wait").and_then(|v| v.as_bool()).unwrap_or(true);
        let wait_timeout = input
            .get("wait_timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);

        let mgr = BgJobManager::global();
        let Some(job) = mgr.get(&job_id) else {
            let data = serde_json::json!({
                "taskId": job_id,
                "retrievalStatus": "not_found",
                "taskStatus": "not_found",
                "taskType": "",
                "output": "",
            });
            return Ok(vec![ToolOutput::Result {
                data: data.clone(),
                result_for_assistant: format_result(&data),
            }]);
        };

        // Snapshot path
        if !wait || job.status() != JobStatus::Running {
            let output = truncate(job.output_snapshot());
            let data = serde_json::json!({
                "taskId": job_id,
                "retrievalStatus": job.status().as_str(),
                "taskStatus": job.status().as_str(),
                "taskType": job.kind.as_str(),
                "output": output,
            });
            return Ok(vec![ToolOutput::Result {
                data: data.clone(),
                result_for_assistant: format_result(&data),
            }]);
        }

        // Wait path
        let final_status = mgr
            .wait(&job_id, Duration::from_millis(wait_timeout))
            .await
            .unwrap_or(JobStatus::Running);
        let retrieval_status = if final_status == JobStatus::Running {
            "timeout"
        } else {
            "completed"
        };
        let output = truncate(job.output_snapshot());
        let data = serde_json::json!({
            "taskId": job_id,
            "retrievalStatus": retrieval_status,
            "taskStatus": final_status.as_str(),
            "taskType": job.kind.as_str(),
            "output": output,
        });
        Ok(vec![ToolOutput::Result {
            data: data.clone(),
            result_for_assistant: format_result(&data),
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let job_id = data.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
        let output = data.get("output").and_then(|v| v.as_str()).unwrap_or("");
        ToolResultMessage {
            title: job_id.to_string(),
            summary: String::new(),
            content: Value::String(if output.is_empty() {
                "(no content)".to_string()
            } else {
                output.to_string()
            }),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        input
            .get("job_id")
            .and_then(|v| v.as_str())
            .unwrap_or("PeekBgJob")
            .to_string()
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_OUTPUT_RETURN {
        let mut end = MAX_OUTPUT_RETURN;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("\n…[truncated]");
    }
    s
}

fn format_result(data: &Value) -> String {
    format!(
        "[PeekBgJob] task_id={} task_type={} status={} retrieval={}\n- output:\n{}",
        data.get("taskId").and_then(|v| v.as_str()).unwrap_or(""),
        data.get("taskType").and_then(|v| v.as_str()).unwrap_or(""),
        data.get("taskStatus")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        data.get("retrievalStatus")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        data.get("output").and_then(|v| v.as_str()).unwrap_or(""),
    )
}

#[cfg(test)]
mod tests {
    use super::super::bg_jobs::{BgJob, JobKind};
    use super::*;

    #[tokio::test]
    async fn peek_unknown_job_returns_not_found() {
        let tool = PeekBgJobTool;
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
            .call(serde_json::json!({"job_id": "nope", "wait": false}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Result { data, .. } = &out[0] else {
            panic!("unexpected variant");
        };
        assert_eq!(data["retrievalStatus"], "not_found");
    }

    #[tokio::test]
    async fn snapshot_returns_current_output() {
        let mgr = BgJobManager::global();
        let id = mgr.next_id(JobKind::Bash);
        let job = BgJob::new(id.clone(), JobKind::Bash, "echo hi".to_string());
        job.append_output("hello\nworld");
        job.mark_done(JobStatus::Done);
        mgr.register(job);

        let tool = PeekBgJobTool;
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
            .call(serde_json::json!({"job_id": id, "wait": false}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Result { data, .. } = &out[0] else {
            panic!("unexpected variant");
        };
        assert_eq!(data["output"], "hello\nworld");
        assert_eq!(data["taskStatus"], "done");
    }
}

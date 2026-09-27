//! StopBgJob tool — terminate a running background job.
//!
//! Equivalent of sema-core's `stop_bg_job`.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::bg_jobs::{BgJobManager, JobStatus};
use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolPermissionInfo, ToolResultMessage};

pub struct StopBgJobTool;

#[async_trait]
impl Tool for StopBgJobTool {
    fn name(&self) -> &str {
        "StopBgJob"
    }

    fn description(&self) -> &str {
        "Stop a running background job started by Bash with background=true."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": {
                    "type": "string",
                    "description": "ID of the background job to terminate."
                }
            },
            "required": ["job_id"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let job_id = input
            .get("job_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mgr = BgJobManager::global();
        let Some(job) = mgr.get(&job_id) else {
            let data = serde_json::json!({
                "taskId": job_id,
                "message": format!("No job found with ID {job_id}."),
                "taskType": "",
                "command": "",
                "stopped": false,
            });
            return Ok(vec![ToolOutput::Result {
                data: data.clone(),
                result_for_assistant: format_result(&data),
            }]);
        };

        if job.status() != JobStatus::Running {
            let data = serde_json::json!({
                "taskId": job_id,
                "message": format!("Job is not active (status: {}).", job.status().as_str()),
                "taskType": job.kind.as_str(),
                "command": job.command,
                "stopped": false,
            });
            return Ok(vec![ToolOutput::Result {
                data: data.clone(),
                result_for_assistant: format_result(&data),
            }]);
        }

        let killed = job.kill().await;
        let data = serde_json::json!({
            "taskId": job_id,
            "message": if killed {
                format!("Job {job_id} stopped ({}).", job.command)
            } else {
                format!("Could not stop job {job_id} ({}) — process may have already exited.", job.command)
            },
            "taskType": job.kind.as_str(),
            "command": job.command,
            "stopped": killed,
        });
        Ok(vec![ToolOutput::Result {
            data: data.clone(),
            result_for_assistant: format_result(&data),
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let job_id = data.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
        let stopped = data
            .get("stopped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let command = data.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let msg = data.get("message").and_then(|v| v.as_str()).unwrap_or("");
        ToolResultMessage {
            title: job_id.to_string(),
            summary: String::new(),
            content: Value::String(if stopped {
                format!("{command} · stopped")
            } else {
                msg.to_string()
            }),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let id = input.get("job_id").and_then(|v| v.as_str()).unwrap_or("");
        format!("StopBgJob: {id}")
    }

    fn gen_tool_permission(&self, input: &Value) -> Option<ToolPermissionInfo> {
        let id = input
            .get("job_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Some(ToolPermissionInfo {
            title: format!("StopBgJob: {id}"),
            content: Value::String(format!("Stop background job {id}")),
        })
    }
}

fn format_result(data: &Value) -> String {
    format!(
        "[StopBgJob] task_id={} task_type={} stopped={}\n- command: {}\n- message: {}",
        data.get("taskId").and_then(|v| v.as_str()).unwrap_or(""),
        data.get("taskType").and_then(|v| v.as_str()).unwrap_or(""),
        data.get("stopped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        data.get("command").and_then(|v| v.as_str()).unwrap_or(""),
        data.get("message").and_then(|v| v.as_str()).unwrap_or(""),
    )
}

#[cfg(test)]
mod tests {
    use super::super::bg_jobs::{BgJob, JobKind};
    use super::*;

    #[tokio::test]
    async fn stop_unknown_job() {
        let tool = StopBgJobTool;
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
            .call(serde_json::json!({"job_id": "missing"}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Result { data, .. } = &out[0] else {
            panic!("unexpected");
        };
        assert_eq!(data["stopped"], false);
    }

    #[tokio::test]
    async fn stop_already_done_job_returns_false() {
        let mgr = BgJobManager::global();
        let id = mgr.next_id(JobKind::Bash);
        let job = BgJob::new(id.clone(), JobKind::Bash, "echo".into());
        job.mark_done(JobStatus::Done);
        mgr.register(job);

        let tool = StopBgJobTool;
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
            .call(serde_json::json!({"job_id": id}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Result { data, .. } = &out[0] else {
            panic!();
        };
        assert_eq!(data["stopped"], false);
    }
}

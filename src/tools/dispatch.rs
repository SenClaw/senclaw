//! Native dispatch tools — DAG task orchestration via DispatchBridge.
//!
//! Migrated from `src/mcp/dispatch_server.rs` (MCP stdio subprocess) to native
//! `Tool` trait implementations. Each tool constructs a `DispatchServer` per
//! call (same pattern as `McpDispatchServer::inner()`), avoiding Send+Sync
//! issues with the persona resolver trait object.
//!
//! Tools:
//! - `DispatchListAgents` — list available agents and personas
//! - `DispatchCreateParent` — create a DAG parent with tasks
//! - `DispatchCreateParentAndRun` — create + block until all tasks finish
//! - `DispatchTask` — wait for a single task's result
//! - `DispatchAllTasks` — wait for all tasks in dependency order

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::mcp::dispatch_server::{
    BuiltinAwarePersonaResolver, CreateParentParams, DispatchAllTasksParams, DispatchServer,
    DispatchTaskParams, FsPersonaResolver, PersonaResolver,
};
use crate::mcp::schedule_server::ToolResult;
use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolPermissionInfo, ToolResultMessage};

// =============================================================================
// Shared config — Clone + Send + Sync, constructs DispatchServer per call
// =============================================================================

/// Configuration for dispatch tools. Mirrors `McpDispatchServer` fields
/// (dispatch_server.rs:1155-1160). Held in `Arc` and shared by all tool instances.
#[derive(Clone)]
pub struct DispatchToolsConfig {
    pub state_path: PathBuf,
    pub admin_folder: String,
    pub agents_config_dir: Option<String>,
}

impl DispatchToolsConfig {
    /// Construct a fresh `DispatchServer` for this call. Mirrors
    /// `McpDispatchServer::inner()` (dispatch_server.rs:1163-1190) but uses
    /// `BuiltinAwarePersonaResolver` so builtin agents are always visible.
    fn make_server(&self) -> DispatchServer {
        let persona_resolver: Option<Box<dyn PersonaResolver>> = {
            let fs = self
                .agents_config_dir
                .as_ref()
                .map(|dir| FsPersonaResolver::from_dir(Path::new(dir)));
            Some(Box::new(BuiltinAwarePersonaResolver::new(fs)) as Box<dyn PersonaResolver>)
        };
        DispatchServer::new(&self.state_path, &self.admin_folder, persona_resolver)
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn tool_result_to_output(r: ToolResult) -> Vec<ToolOutput> {
    let text = if r.is_error {
        format!("Error: {}", r.content)
    } else {
        r.content.clone()
    };
    vec![ToolOutput::Result {
        data: serde_json::json!({ "content": r.content, "isError": r.is_error }),
        result_for_assistant: text,
    }]
}

fn err_output(msg: &str) -> Vec<ToolOutput> {
    vec![ToolOutput::Result {
        data: serde_json::json!({ "content": msg, "isError": true }),
        result_for_assistant: format!("Error: {msg}"),
    }]
}

// =============================================================================
// DispatchListAgents
// =============================================================================

pub struct DispatchListAgentsTool {
    config: Arc<DispatchToolsConfig>,
}

impl DispatchListAgentsTool {
    pub fn new(config: Arc<DispatchToolsConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for DispatchListAgentsTool {
    fn name(&self) -> &str {
        "DispatchListAgents"
    }

    fn description(&self) -> &str {
        "List agents valid for dispatch subtasks. Shows registered persistent \
         agents and virtual personas. Use this before creating a dispatch parent \
         to discover available agent names."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn should_defer(&self) -> bool {
        true
    }

    async fn call(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let server = self.config.make_server();
        Ok(tool_result_to_output(server.list_agents()))
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "DispatchListAgents".to_string(),
            summary: "Listed dispatch agents".to_string(),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, _input: &Value) -> String {
        "List dispatch agents".to_string()
    }
}

// =============================================================================
// DispatchCreateParent
// =============================================================================

pub struct DispatchCreateParentTool {
    config: Arc<DispatchToolsConfig>,
}

impl DispatchCreateParentTool {
    pub fn new(config: Arc<DispatchToolsConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for DispatchCreateParentTool {
    fn name(&self) -> &str {
        "DispatchCreateParent"
    }

    fn description(&self) -> &str {
        "Create a parent dispatch with multiple tasks forming a DAG. Subtasks \
         start in the daemon scheduler; call DispatchAllTasks afterwards to \
         wait for results. Use dependsOn to define task ordering."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "One-line summary of the overall objective."
                },
                "tasks": {
                    "type": "array",
                    "description": "Subtasks to dispatch.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {
                                "type": "string",
                                "description": "Unique label for this task (used in dependsOn references)."
                            },
                            "agentName": {
                                "type": "string",
                                "description": "Agent name or persona:{name} to assign."
                            },
                            "prompt": {
                                "type": "string",
                                "description": "Self-contained prompt for the sub-agent."
                            },
                            "dependsOn": {
                                "type": "array",
                                "items": {"type": "string"},
                                "default": [],
                                "description": "Labels of tasks that must complete before this one starts."
                            },
                            "isolation": {
                                "type": "string",
                                "enum": ["none", "worktree"],
                                "default": "none",
                                "description": "worktree = run in an isolated git worktree (branch senclaw/<parent>-<label>); the shared checkout stays untouched until a person merges. Requires a git repository."
                            }
                        },
                        "required": ["agentName", "prompt"]
                    }
                },
                "timeoutSeconds": {
                    "type": "integer",
                    "description": "Per-task timeout in seconds (default: 900)."
                }
            },
            "required": ["goal", "tasks"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn should_defer(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let params: CreateParentParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(err_output(&format!("Invalid input: {e}"))),
        };
        let server = self.config.make_server();
        Ok(tool_result_to_output(server.create_parent(
            &params.goal,
            params.tasks,
            params.timeout_seconds,
        )))
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "DispatchCreateParent".to_string(),
            summary: "Created dispatch parent".to_string(),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let goal = input.get("goal").and_then(|v| v.as_str()).unwrap_or("…");
        let short: String = goal.chars().take(40).collect();
        format!("Dispatch: {short}")
    }

    fn gen_tool_permission(&self, input: &Value) -> Option<ToolPermissionInfo> {
        let goal = input
            .get("goal")
            .and_then(|v| v.as_str())
            .unwrap_or("(no goal)");
        let task_count = input
            .get("tasks")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        Some(ToolPermissionInfo {
            title: format!("Dispatch {task_count} task(s)"),
            content: serde_json::json!({ "goal": goal, "taskCount": task_count }),
        })
    }
}

// =============================================================================
// DispatchCreateParentAndRun
// =============================================================================

pub struct DispatchCreateParentAndRunTool {
    config: Arc<DispatchToolsConfig>,
}

impl DispatchCreateParentAndRunTool {
    pub fn new(config: Arc<DispatchToolsConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for DispatchCreateParentAndRunTool {
    fn name(&self) -> &str {
        "DispatchCreateParentAndRun"
    }

    fn description(&self) -> &str {
        "Create a dispatch parent and block until every subtask finishes in \
         dependency order. Same args as DispatchCreateParent. Prefer this when \
         the user wants the full pipeline without a second tool call."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "One-line summary of the overall objective."
                },
                "tasks": {
                    "type": "array",
                    "description": "Subtasks to dispatch.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {
                                "type": "string",
                                "description": "Unique label for this task."
                            },
                            "agentName": {
                                "type": "string",
                                "description": "Agent name or persona:{name} to assign."
                            },
                            "prompt": {
                                "type": "string",
                                "description": "Self-contained prompt for the sub-agent."
                            },
                            "dependsOn": {
                                "type": "array",
                                "items": {"type": "string"},
                                "default": [],
                                "description": "Labels of prerequisite tasks."
                            }
                        },
                        "required": ["agentName", "prompt"]
                    }
                },
                "timeoutSeconds": {
                    "type": "integer",
                    "description": "Per-task timeout in seconds (default: 900)."
                }
            },
            "required": ["goal", "tasks"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn should_defer(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let params: CreateParentParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(err_output(&format!("Invalid input: {e}"))),
        };
        let server = self.config.make_server();
        let result = tokio::select! {
            r = server.create_parent_and_run(&params.goal, params.tasks, params.timeout_seconds) => r,
            _ = ctx.abort.cancelled() => {
                return Ok(err_output("Dispatch cancelled by user"));
            }
        };
        Ok(tool_result_to_output(result))
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "DispatchCreateParentAndRun".to_string(),
            summary: "Dispatched and ran tasks".to_string(),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let goal = input.get("goal").and_then(|v| v.as_str()).unwrap_or("…");
        let short: String = goal.chars().take(40).collect();
        format!("Dispatch & run: {short}")
    }

    fn gen_tool_permission(&self, input: &Value) -> Option<ToolPermissionInfo> {
        let goal = input
            .get("goal")
            .and_then(|v| v.as_str())
            .unwrap_or("(no goal)");
        let task_count = input
            .get("tasks")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        Some(ToolPermissionInfo {
            title: format!("Dispatch & run {task_count} task(s)"),
            content: serde_json::json!({ "goal": goal, "taskCount": task_count }),
        })
    }
}

// =============================================================================
// DispatchTask — wait for a single task result
// =============================================================================

pub struct DispatchTaskTool {
    config: Arc<DispatchToolsConfig>,
}

impl DispatchTaskTool {
    pub fn new(config: Arc<DispatchToolsConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for DispatchTaskTool {
    fn name(&self) -> &str {
        "DispatchTask"
    }

    fn description(&self) -> &str {
        "Wait for a single dispatch task to complete and return its result. \
         Specify the parentId and taskLabel to identify the task."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "parentId": {
                    "type": "string",
                    "description": "Parent dispatch ID (e.g. p-20260604-0001)."
                },
                "taskLabel": {
                    "type": "string",
                    "description": "Label of the task to wait for."
                },
                "timeoutSeconds": {
                    "type": "integer",
                    "description": "Override timeout in seconds."
                }
            },
            "required": ["parentId", "taskLabel"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn should_defer(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let params: DispatchTaskParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(err_output(&format!("Invalid input: {e}"))),
        };
        let server = self.config.make_server();
        let result = tokio::select! {
            r = server.dispatch_task(&params.parent_id, &params.task_label, params.timeout_seconds) => r,
            _ = ctx.abort.cancelled() => {
                return Ok(err_output("Dispatch task wait cancelled"));
            }
        };
        Ok(tool_result_to_output(result))
    }

    fn gen_tool_result_message(&self, data: &Value, input: &Value) -> ToolResultMessage {
        let label = input
            .get("taskLabel")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        ToolResultMessage {
            title: "DispatchTask".to_string(),
            summary: format!("Task \"{label}\" result"),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let label = input
            .get("taskLabel")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        format!("Wait task: {label}")
    }
}

// =============================================================================
// DispatchAllTasks — wait for all tasks in dependency order
// =============================================================================

pub struct DispatchAllTasksTool {
    config: Arc<DispatchToolsConfig>,
}

impl DispatchAllTasksTool {
    pub fn new(config: Arc<DispatchToolsConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for DispatchAllTasksTool {
    fn name(&self) -> &str {
        "DispatchAllTasks"
    }

    fn description(&self) -> &str {
        "Run every task under a parent in dependency order and return combined \
         results. Stops on first error. Prefer this over calling DispatchTask \
         repeatedly."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "parentId": {
                    "type": "string",
                    "description": "Parent dispatch ID (e.g. p-20260604-0001)."
                },
                "timeoutSeconds": {
                    "type": "integer",
                    "description": "Override timeout in seconds."
                }
            },
            "required": ["parentId"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn should_defer(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let params: DispatchAllTasksParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return Ok(err_output(&format!("Invalid input: {e}"))),
        };
        let server = self.config.make_server();
        let result = tokio::select! {
            r = server.dispatch_all_tasks(&params.parent_id, params.timeout_seconds) => r,
            _ = ctx.abort.cancelled() => {
                return Ok(err_output("Dispatch all-tasks wait cancelled"));
            }
        };
        Ok(tool_result_to_output(result))
    }

    fn gen_tool_result_message(&self, data: &Value, input: &Value) -> ToolResultMessage {
        let pid = input
            .get("parentId")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        ToolResultMessage {
            title: "DispatchAllTasks".to_string(),
            summary: format!("All tasks for {pid}"),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let pid = input
            .get("parentId")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        format!("Run all tasks: {pid}")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Arc<DispatchToolsConfig> {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_path = tmp.path().join("dispatch-state.json");
        std::fs::write(&state_path, "{}").unwrap();
        // Keep tmp alive by leaking it (test-only)
        let path = state_path.clone();
        std::mem::forget(tmp);
        Arc::new(DispatchToolsConfig {
            state_path: path,
            admin_folder: "test-agent".into(),
            agents_config_dir: None,
        })
    }

    #[test]
    fn tool_names_are_correct() {
        let cfg = test_config();
        assert_eq!(
            DispatchListAgentsTool::new(cfg.clone()).name(),
            "DispatchListAgents"
        );
        assert_eq!(
            DispatchCreateParentTool::new(cfg.clone()).name(),
            "DispatchCreateParent"
        );
        assert_eq!(
            DispatchCreateParentAndRunTool::new(cfg.clone()).name(),
            "DispatchCreateParentAndRun"
        );
        assert_eq!(DispatchTaskTool::new(cfg.clone()).name(), "DispatchTask");
        assert_eq!(DispatchAllTasksTool::new(cfg).name(), "DispatchAllTasks");
    }

    #[test]
    fn all_dispatch_tools_are_deferred() {
        let cfg = test_config();
        assert!(DispatchListAgentsTool::new(cfg.clone()).should_defer());
        assert!(DispatchCreateParentTool::new(cfg.clone()).should_defer());
        assert!(DispatchCreateParentAndRunTool::new(cfg.clone()).should_defer());
        assert!(DispatchTaskTool::new(cfg.clone()).should_defer());
        assert!(DispatchAllTasksTool::new(cfg).should_defer());
    }

    #[test]
    fn list_agents_is_read_only() {
        let cfg = test_config();
        assert!(DispatchListAgentsTool::new(cfg.clone()).is_read_only());
        // Create tools are NOT read-only
        assert!(!DispatchCreateParentTool::new(cfg).is_read_only());
    }

    #[tokio::test]
    async fn list_agents_includes_builtins() {
        let cfg = test_config();
        let tool = DispatchListAgentsTool::new(cfg);
        let ctx = ToolContext {
            agent_id: "test",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let out = tool.call(serde_json::json!({}), &ctx).await.unwrap();
        assert_eq!(out.len(), 1);
        match &out[0] {
            ToolOutput::Result {
                result_for_assistant,
                ..
            } => {
                // Must list builtin personas even with empty FS dir
                assert!(
                    result_for_assistant.contains("persona:researcher"),
                    "Expected builtin researcher, got: {result_for_assistant}"
                );
                assert!(
                    result_for_assistant.contains("persona:creator"),
                    "Expected builtin creator, got: {result_for_assistant}"
                );
                assert!(
                    result_for_assistant.contains("persona:architect"),
                    "Expected builtin architect, got: {result_for_assistant}"
                );
            }
            _ => panic!("Expected Result output"),
        }
    }

    #[test]
    fn builtin_resolver_includes_all_builtins() {
        let resolver = crate::mcp::dispatch_server::BuiltinAwarePersonaResolver::new(None);
        let list = resolver.list();
        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        assert!(
            names.contains(&"researcher"),
            "missing researcher: {names:?}"
        );
        assert!(names.contains(&"creator"), "missing creator: {names:?}");
        assert!(names.contains(&"architect"), "missing architect: {names:?}");
    }

    #[test]
    fn builtin_resolver_fs_overrides_builtin() {
        // If FS has a "researcher" persona, it takes priority over the builtin.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("researcher.md"),
            "---\nname: researcher\ndescription: Custom researcher\n---\nBody.",
        )
        .unwrap();
        let fs = FsPersonaResolver::from_dir(tmp.path());
        let resolver = crate::mcp::dispatch_server::BuiltinAwarePersonaResolver::new(Some(fs));
        let r = resolver.get("researcher").expect("should find researcher");
        assert_eq!(r.description, "Custom researcher");
        // No duplicates
        let count = resolver
            .list()
            .iter()
            .filter(|p| p.name == "researcher")
            .count();
        assert_eq!(count, 1, "should not have duplicate researcher");
    }

    #[test]
    fn builtin_resolver_get_case_insensitive() {
        let resolver = crate::mcp::dispatch_server::BuiltinAwarePersonaResolver::new(None);
        assert!(resolver.get("Researcher").is_some());
        assert!(resolver.get("CREATOR").is_some());
        assert!(resolver.get("nonexistent").is_none());
    }

    #[test]
    fn tool_result_to_output_ok() {
        let r = ToolResult {
            content: "hello".into(),
            is_error: false,
        };
        let out = tool_result_to_output(r);
        match &out[0] {
            ToolOutput::Result {
                result_for_assistant,
                ..
            } => assert_eq!(result_for_assistant, "hello"),
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn tool_result_to_output_error() {
        let r = ToolResult {
            content: "boom".into(),
            is_error: true,
        };
        let out = tool_result_to_output(r);
        match &out[0] {
            ToolOutput::Result {
                result_for_assistant,
                ..
            } => assert!(result_for_assistant.starts_with("Error:")),
            _ => panic!("expected Result"),
        }
    }
}

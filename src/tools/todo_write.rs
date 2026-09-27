//! TodoWrite tool — agent todo list management.
//!
//! Port of TS `node_modules/sema-core/dist/tools/TodoWrite/`.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::events::EngineEvent;
use crate::zen_core::state::StateManager;
use crate::zen_core::{TodosUpdateItem, Tool, ToolContext, ToolOutput};

pub struct TodoWriteTool {
    state: Arc<Mutex<StateManager>>,
}

impl TodoWriteTool {
    pub fn new(state: Arc<Mutex<StateManager>>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "TodoWrite"
    }

    fn description(&self) -> &str {
        "Create and manage a structured task list for your current coding session"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The updated todo list",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "minLength": 1},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            },
                            "activeForm": {"type": "string", "minLength": 1}
                        },
                        "required": ["content", "status", "activeForm"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        let todos = input.get("todos").and_then(|v| v.as_array());
        let Some(todos) = todos else {
            return Err("todos array is required".to_string());
        };
        if todos.is_empty() {
            return Ok(());
        }
        let in_progress: Vec<_> = todos
            .iter()
            .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("in_progress"))
            .collect();
        if in_progress.len() > 1 {
            return Err("Only one task can be in_progress at a time".to_string());
        }
        for (i, todo) in todos.iter().enumerate() {
            let content = todo.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let active = todo
                .get("activeForm")
                .and_then(|a| a.as_str())
                .unwrap_or("");
            if content.trim().is_empty() {
                return Err(format!("Todo at index {i} has empty content"));
            }
            if active.trim().is_empty() {
                return Err(format!("Todo at index {i} has empty activeForm"));
            }
        }
        Ok(())
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let todos = input.get("todos").and_then(|v| v.as_array());

        let items: Vec<TodosUpdateItem> = match todos {
            Some(arr) if !arr.is_empty() => arr
                .iter()
                .map(|t| TodosUpdateItem {
                    content: t
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string(),
                    status: t
                        .get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("pending")
                        .to_string(),
                    active_form: t
                        .get("activeForm")
                        .and_then(|a| a.as_str())
                        .map(String::from),
                })
                .collect(),
            _ => Vec::new(),
        };

        if items.is_empty() {
            // Check for incomplete tasks before clearing
            let mut state = self.state.lock().unwrap();
            let current = state.todos(ctx.agent_id);
            let incomplete: Vec<_> = current
                .iter()
                .filter(|t| t.status == "pending" || t.status == "in_progress")
                .collect();

            if !incomplete.is_empty() {
                let list = current
                    .iter()
                    .enumerate()
                    .map(|(i, t)| format!("{}. [{}] {}", i + 1, t.status, t.content))
                    .collect::<Vec<_>>()
                    .join("\n");
                return Ok(vec![ToolOutput::Result {
                    data: serde_json::json!([]),
                    result_for_assistant: format!(
                        "Error: TodoWrite received an empty todo list, but {} task(s) are still incomplete. This is likely a format error. Current task list:\n{}\n\nPlease regenerate the TodoWrite call with the complete todo list and correct status updates.",
                        incomplete.len(), list
                    ),
                }]);
            }

            let items = vec![];
            state.update_todos_intelligently(ctx.agent_id, items.clone());
            if let Some(bus) = ctx.event_bus {
                bus.emit(EngineEvent::TodosUpdate(items));
            }
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!([]),
                result_for_assistant: "Todo list cleared. All tasks are complete — summarize your work and report the results.".into(),
            }]);
        }

        let mut state = self.state.lock().unwrap();
        state.update_todos_intelligently(ctx.agent_id, items.clone());
        if let Some(bus) = ctx.event_bus {
            tracing::info!(
                "[TodoWrite] Emitting TodosUpdate for {} ({} items)",
                ctx.agent_id,
                items.len()
            );
            bus.emit(EngineEvent::TodosUpdate(items.clone()));
        } else {
            tracing::warn!("[TodoWrite] No event_bus in ToolContext — todos update NOT broadcast");
        }

        Ok(vec![ToolOutput::Result {
            data: serde_json::json!(items),
            result_for_assistant: "Todos updated successfully. Continue with any remaining tasks and keep using todo list to track progress if applicable.".into(),
        }])
    }

    fn gen_tool_result_message(
        &self,
        _data: &Value,
        _input: &Value,
    ) -> crate::zen_core::ToolResultMessage {
        crate::zen_core::ToolResultMessage {
            title: "TodoWrite".into(),
            summary: "Todo list".into(),
            content: _data.clone(),
        }
    }

    fn get_display_title(&self, _input: &Value) -> String {
        "Update todo list".into()
    }
}

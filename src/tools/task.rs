//! Task tool — launches specialized subagents for complex, multi-step tasks.
//!
//! Port of TS `node_modules/sema-core/dist/tools/Task/Task.js`.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use crate::zen_core::conversation::{self, QueryConfig};
use crate::zen_core::run_tools::PermissionChecker;
use crate::zen_core::state::StateManager;
use crate::zen_core::{
    create_user_message, ContentBlock, EngineEvent, EventBus, ModelProfile, TaskAgentEndData,
    TaskAgentStartData, Tool, ToolContext, ToolOutput, ToolResultMessage,
};

/// Agent configuration matching TS AgentConfig.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub name: String,
    pub description: String,
    /// "*" for all tools, or a list of tool names.
    pub tools: Vec<String>,
    /// "main" or "quick"
    pub model: String,
    /// The system prompt content.
    pub prompt: String,
    pub locate: String,
}

/// Built-in default agent configs (mirrors TS defaultBuiltInAgentsConfs).
pub fn default_agent_configs() -> Vec<AgentConfig> {
    vec![
        AgentConfig {
            name: "general-purpose".into(),
            description:
                "General-purpose agent for researching complex questions, searching for code, and executing multi-step tasks."
                    .into(),
            tools: vec![
                "Bash", "Edit", "Glob", "Grep", "NotebookEdit", "Read", "Skill", "TodoWrite",
                "Write",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            model: "main".into(),
            prompt: AGENT_PROMPT_GENERAL.into(),
            locate: "builtin".into(),
        },
    ]
}

const AGENT_PROMPT_GENERAL: &str = "You are a helpful AI assistant with access to tools.";

/// Closure that returns the current main-agent tool list (already filtered
/// by `use_tools` / Plan mode / cowork rules). Called per subagent spawn so
/// the subagent inherits live engine state instead of a stale snapshot.
pub type ToolResolver = Arc<dyn Fn() -> Vec<Arc<dyn Tool>> + Send + Sync>;

/// Closure that returns the model profile to use for a spawned subagent.
/// Resolved per spawn so subagents inherit the engine's live model selection
/// (e.g. a per-group LLM override) rather than a snapshot taken at construction.
pub type ProfileResolver = Arc<dyn Fn() -> ModelProfile + Send + Sync>;

pub struct TaskTool {
    http_client: Client,
    event_bus: EventBus,
    state: Arc<std::sync::Mutex<StateManager>>,
    permission_checker: Arc<dyn PermissionChecker>,
    agent_configs: Vec<AgentConfig>,
    working_dir: String,
    agent_data_dir: String,
    /// Resolves the current main-agent tool list at spawn time. Calling this
    /// every subagent call means the subagent sees `use_tools` / Plan-mode
    /// updates that happened after TaskTool was constructed.
    tools_resolver: ToolResolver,
    /// Resolves the model profile at spawn time so subagents inherit the
    /// engine's live model selection (per-group LLM override).
    profile_resolver: ProfileResolver,
}

impl TaskTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        http_client: Client,
        event_bus: EventBus,
        state: Arc<std::sync::Mutex<StateManager>>,
        permission_checker: Arc<dyn PermissionChecker>,
        agent_configs: Vec<AgentConfig>,
        working_dir: String,
        agent_data_dir: String,
        tools_resolver: ToolResolver,
        profile_resolver: ProfileResolver,
    ) -> Self {
        Self {
            http_client,
            event_bus,
            state,
            permission_checker,
            agent_configs,
            working_dir,
            agent_data_dir,
            tools_resolver,
            profile_resolver,
        }
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "Task"
    }

    fn description(&self) -> &str {
        "Launch a new agent to handle complex, multi-step tasks"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "A brief (3-5 word) description of the task"
                },
                "prompt": {
                    "type": "string",
                    "description": "The task for the agent to perform"
                },
                "subagent_type": {
                    "type": "string",
                    "description": "The name of specialized agent to use for the task"
                },
                "isolation": {
                    "type": "string",
                    "enum": ["none", "worktree"],
                    "description": "worktree = the subagent edits its own git worktree (branch senclaw/task-<id>) instead of this chat's working directory; the result reports the branch and diff. Only in a git repository."
                }
            },
            "required": ["description", "prompt", "subagent_type"]
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
        let subagent_type = input
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if subagent_type.trim().is_empty() {
            return Err("subagent_type is required".to_string());
        }
        if !self
            .agent_configs
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(subagent_type))
        {
            let available: Vec<&str> = self.agent_configs.iter().map(|a| a.name.as_str()).collect();
            return Err(format!(
                "Unknown agent type: {subagent_type}. Available types: {}",
                available.join(", ")
            ));
        }
        Ok(())
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let start = Instant::now();
        let task_id = Uuid::new_v4().to_string();

        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let subagent_type = input
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // 1. Find agent config
        let agent_config = match self
            .agent_configs
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(&subagent_type))
        {
            Some(c) => c.clone(),
            None => {
                let available: Vec<&str> =
                    self.agent_configs.iter().map(|a| a.name.as_str()).collect();
                let error_msg = format!(
                    "Unknown agent type: {subagent_type}. Available: {}",
                    available.join(", ")
                );
                return Ok(vec![ToolOutput::Result {
                    data: serde_json::json!({
                        "agentType": subagent_type,
                        "result": error_msg,
                        "durationMs": start.elapsed().as_millis() as u64,
                    }),
                    result_for_assistant: error_msg,
                }]);
            }
        };

        debug!(
            "Starting {} agent with prompt: {}",
            agent_config.name, prompt
        );

        // 2. Emit task:agent:start
        self.event_bus
            .emit(EngineEvent::TaskAgentStart(TaskAgentStartData {
                task_id: task_id.clone(),
                subagent_type: agent_config.name.clone(),
                description: description.clone(),
                prompt: prompt.clone(),
            }));

        // 3. Build subagent system prompt
        let system_prompt = agent_config.prompt.clone();

        // 4. Determine tools for subagent. The resolver returns the current
        //    main-agent tool list (already filtered by use_tools, Plan mode,
        //    cowork rules). On top of that we layer:
        //    (a) `agent_config.tools` whitelist ("*" = inherit all)
        //    (b) `SUBAGENT_EXCLUDED_TOOLS` — token-saving guard: subagents never
        //        see Task / PeekBgJob / StopBgJob / AskUser* / ExitPlanMode /
        //        TodoWrite (mirrors sema-core's `SUBAGENT_EXCLUDED_TOOLS`).
        use crate::zen_core::prompt::SUBAGENT_EXCLUDED_TOOLS;
        let main_tools = (self.tools_resolver)();
        let subagent_tools: Vec<Arc<dyn Tool>> = if agent_config.tools.iter().any(|t| t == "*") {
            main_tools
                .iter()
                .filter(|t| !SUBAGENT_EXCLUDED_TOOLS.contains(&t.name()))
                .cloned()
                .collect()
        } else {
            let allowed: std::collections::HashSet<&str> =
                agent_config.tools.iter().map(|s| s.as_str()).collect();
            main_tools
                .iter()
                .filter(|t| {
                    allowed.contains(t.name()) && !SUBAGENT_EXCLUDED_TOOLS.contains(&t.name())
                })
                .cloned()
                .collect()
        };

        // 5. Build user message with the prompt
        let user_msg = create_user_message(vec![ContentBlock::Text {
            text: prompt.clone(),
        }]);
        let messages = vec![user_msg];

        // 6. Get shared abort token
        let abort = ctx.abort.clone();

        // 7. Run subagent conversation.
        // Wrap the resolved subagent tool list into a resolver so QueryConfig
        // matches the new `ToolsResolver` shape. Subagents don't dynamically
        // discover (ToolSearch is in SUBAGENT_EXCLUDED), so a static
        // captured list is correct.
        let subagent_tools_static = subagent_tools.clone();
        let subagent_tools_resolver: crate::zen_core::conversation::ToolsResolver =
            Arc::new(move || subagent_tools_static.clone());
        // Optional worktree isolation: the subagent gets its own checkout of
        // the chat's repository, so a parallel helper cannot collide with the
        // main agent's edits. Falls back to the shared directory (reported)
        // when the working directory is not a git repository.
        let wants_worktree = input.get("isolation").and_then(|v| v.as_str()) == Some("worktree");
        let mut subagent_working_dir = self.working_dir.clone();
        let mut worktree_info: Option<crate::worktree::WorktreeInfo> = None;
        let mut isolation_note: Option<String> = None;
        if wants_worktree {
            let short_id: String = task_id.chars().take(8).collect();
            match crate::worktree::create(
                std::path::Path::new(&self.working_dir),
                &format!("task-{short_id}"),
                &format!("task:{task_id}"),
            ) {
                Ok(info) => {
                    subagent_working_dir = info.path.clone();
                    worktree_info = Some(info);
                }
                Err(e) => isolation_note = Some(format!("isolation=worktree ignored: {e}")),
            }
        }

        let query_config = QueryConfig {
            agent_id: task_id.clone(),
            working_dir: subagent_working_dir,
            agent_data_dir: self.agent_data_dir.clone(),
            system_prompt,
            tools: subagent_tools_resolver,
            http_client: self.http_client.clone(),
            event_bus: self.event_bus.clone(),
            response_registry: None,
            permission_checker: self.permission_checker.clone(),
            profile: (self.profile_resolver)(),
            thinking: false,
            stream: false,
            is_subagent: true,
            hook_manager: None,
            hook_client: None,
            hook_profile: None,
            session_id: String::new(),
            enable_cache: false,
            // Subagents are best-effort one-shot helpers with their own
            // scratch context, not the main loop a user watches — no status
            // line for them.
            agent_status: false,
            // Subagents spawned via the Task tool run as plain Agent mode —
            // they're best-effort one-shot helpers, not Plan/Dag-style
            // workflows that need task_done enforcement.
            agent_mode: crate::zen_core::AgentMode::Agent,
            max_turns_override: None,
            // Subagents never receive mid-turn user injections.
            pending_inject: None,
        };

        let result = conversation::query(messages, &query_config, &abort).await;

        // 8. Clean up subagent state
        {
            let mut state = self.state.lock().unwrap();
            state.clear_agent(&task_id);
        }

        // 9. Handle result
        match result {
            Ok(final_messages) => {
                let result_text = extract_last_assistant_text(&final_messages);
                let is_interrupted = abort.is_cancelled();
                let duration_ms = start.elapsed().as_millis() as u64;

                self.event_bus
                    .emit(EngineEvent::TaskAgentEnd(TaskAgentEndData {
                        task_id: task_id.clone(),
                        status: if is_interrupted {
                            "interrupted".to_string()
                        } else {
                            "completed".to_string()
                        },
                        content: format!(
                            "{} agent {}",
                            agent_config.name,
                            if is_interrupted {
                                "interrupted"
                            } else {
                                "completed"
                            }
                        ),
                    }));

                // Worktree runs report where their edits are; the chat's own
                // checkout was not touched.
                let mut result_text = result_text;
                if let Some(wt) = &worktree_info {
                    let summary = crate::worktree::summary(std::path::Path::new(&wt.path));
                    result_text.push_str(&format!(
                        "\n\n<worktree>\n{summary}\npath: {}\nThis chat's working directory is unchanged; review the branch diff before merging (`POST /api/worktrees/merge`).\n</worktree>",
                        wt.path
                    ));
                } else if let Some(note) = &isolation_note {
                    result_text.push_str(&format!("\n\n<note>{note}</note>"));
                }
                Ok(vec![ToolOutput::Result {
                    data: serde_json::json!({
                        "agentType": agent_config.name,
                        "result": result_text,
                        "durationMs": duration_ms,
                        "worktree": worktree_info,
                    }),
                    result_for_assistant: result_text,
                }])
            }
            Err(e) => {
                let is_interrupted = abort.is_cancelled();
                let duration_ms = start.elapsed().as_millis() as u64;
                let error_msg = if is_interrupted {
                    format!("{} agent interrupted", agent_config.name)
                } else {
                    format!("Subagent execution failed: {e}")
                };

                self.event_bus
                    .emit(EngineEvent::TaskAgentEnd(TaskAgentEndData {
                        task_id,
                        status: "failed".to_string(),
                        content: error_msg.clone(),
                    }));

                Ok(vec![ToolOutput::Result {
                    data: serde_json::json!({
                        "agentType": agent_config.name,
                        "result": error_msg,
                        "durationMs": duration_ms,
                    }),
                    result_for_assistant: error_msg,
                }])
            }
        }
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let agent_type = data
            .get("agentType")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown");
        ToolResultMessage {
            title: format!("{agent_type} agent"),
            summary: format!("{agent_type} agent completed"),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("Task")
            .to_string()
    }
}

fn extract_last_assistant_text(messages: &[crate::zen_core::Message]) -> String {
    for msg in messages.iter().rev() {
        if msg.msg_type == "assistant" {
            let text: String = msg
                .message
                .content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::Text { text } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                return text;
            }
        }
    }
    "No output from subagent.".to_string()
}

impl std::fmt::Debug for TaskTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskTool")
            .field("agent_configs", &self.agent_configs.len())
            .finish()
    }
}

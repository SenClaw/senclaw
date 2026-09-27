//! Tool execution engine.
//!
//! Validates input, checks permissions, and executes tools (concurrently
//! for read-only tools, serially for write tools).
//!
//! Port of TS `RunTools.ts`.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::hooks::{
    self as zen_hooks, ExecuteHooksOptions, HookEvent, HookInput, HookInputBase, HookManager,
    OutputFilterInput, PermissionRequestInput, PostToolUseInput, PrePermissionInput,
    PreToolUseInput,
};
use super::*;

/// Outcome of running the `PrePermission` hook chain. `Passthrough` means
/// no hook expressed an opinion, so the normal user-prompted permission
/// flow should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrePermissionDecision {
    Allow,
    Deny,
    Passthrough,
}

/// The model's own one-line account of what a step is doing, read off the
/// tool's `description` input.
///
/// `Bash` and `Task` both *require* that parameter, so every shell step and
/// every subagent step carries a sentence a person can read. The clients show
/// it as the step's headline and fall back to the tool's verb when it is
/// empty, which is why this returns an empty string rather than a guess:
/// "Ran a command" is a truthful fallback, an invented description is not.
///
/// Deliberately keyed on the field and not on a per-tool table — any tool that
/// grows a `description` input is picked up with nothing to update here.
pub fn step_description(input: &serde_json::Value) -> String {
    input
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| crate::util::text::truncate_on_char_boundary(s, 240).to_string())
        .unwrap_or_default()
}

/// The *shape* of a tool call's arguments: name -> JSON type, never a value.
///
/// Attached to every `ToolExecutionError` so the failure ledger
/// ([`crate::failures`]) can tell a call that was missing an argument from one
/// whose argument was wrong — the single most common thing a model gets wrong
/// about a tool. Values are excluded on purpose: they are a path, a query, a
/// customer name, and the ledger is durable storage that outlives the chat.
///
/// Empty strings, arrays and objects are reported as their own type, because
/// "passed `host` as an empty string" and "did not pass `host`" are different
/// mistakes that would otherwise look identical.
fn arg_shape(input: &serde_json::Value) -> std::collections::BTreeMap<String, String> {
    let Some(map) = input.as_object() else {
        return std::collections::BTreeMap::new();
    };
    map.iter()
        .map(|(k, v)| {
            let ty = match v {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "bool",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::String(s) if s.is_empty() => "string:empty",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Array(a) if a.is_empty() => "array:empty",
                serde_json::Value::Array(_) => "array",
                serde_json::Value::Object(o) if o.is_empty() => "object:empty",
                serde_json::Value::Object(_) => "object",
            };
            (k.clone(), ty.to_string())
        })
        .collect()
}

/// Rewrite every path-shaped input field into an absolute path.
///
/// Fields come from [`Tool::path_fields`], so a tool opts in and an MCP tool
/// whose `path` means something else is untouched. Resolution is
/// [`crate::util::paths::resolve_in_workspace`]: `~` first, absolute as given,
/// relative joined onto the working directory. An empty working directory
/// (a one-shot with none set) leaves the input alone.
fn resolve_path_inputs(
    tool: &dyn Tool,
    mut input: serde_json::Value,
    working_dir: &str,
) -> serde_json::Value {
    let fields = tool.path_fields();
    if fields.is_empty() || working_dir.is_empty() {
        return input;
    }
    let Some(map) = input.as_object_mut() else {
        return input;
    };
    for field in fields {
        let Some(raw) = map.get(*field).and_then(|v| v.as_str()) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let abs = crate::util::paths::resolve_in_workspace(raw, working_dir);
        map.insert(
            (*field).to_string(),
            serde_json::Value::String(abs.to_string_lossy().to_string()),
        );
    }
    input
}

/// Argument names that say what a call was *for*, in the order we prefer them.
///
/// Ordered by how much they tell a reader: a query or a URL identifies the
/// call, a selector or a tab id barely does. The list is deliberately short —
/// a long one starts matching bookkeeping fields and printing noise.
const HINT_KEYS: &[&str] = &[
    "query",
    "url",
    "command",
    "prompt",
    "file_path",
    "path",
    "pattern",
    "skill",
    "question",
    "topic",
    "text",
    "script",
    "selector",
    "name",
    "id",
    "tab_id",
];

/// A short, human-readable value read off a tool's input, saying what the call
/// was for: the query searched, the URL opened, the file touched.
///
/// This exists because a tool's display name alone is not an account of a
/// step. "Browser Search" tells a reader nothing; "Browser Search: giá vàng
/// hôm nay" tells them the whole step. Only scalar values are accepted —
/// serialising a nested object here is how the raw JSON got into the step list
/// in the first place.
pub fn argument_hint(input: &serde_json::Value) -> Option<String> {
    let obj = input.as_object()?;
    for key in HINT_KEYS {
        let Some(value) = obj.get(*key) else { continue };
        let text = match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(_) | serde_json::Value::Null => continue,
            // An array or object is structure, not a label.
            _ => continue,
        };
        let one_line = collapse_ws(&text);
        if one_line.is_empty() {
            continue;
        }
        return Some(clip(&one_line, 120));
    }
    None
}

/// A short excerpt of what a call returned, for the calls whose input says
/// nothing useful.
///
/// `browser_extract_text` takes a tab id and a selector; neither answers "what
/// did it extract". The first line of the result does. Unwraps the MCP content
/// envelope first, because the envelope is JSON wrapping JSON and printing it
/// raw is exactly the failure this replaces.
pub fn result_excerpt(data: &serde_json::Value) -> Option<String> {
    let inner = crate::scheduler::watch::normalize_tool_result(data);
    let text = match &inner {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        // Still structured after unwrapping: there is no sentence in here to
        // show, and dumping the object is the bug.
        _ => return None,
    };
    let one_line = collapse_ws(&text);
    if one_line.is_empty() {
        return None;
    }
    Some(clip(&one_line, 100))
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    format!(
        "{}…",
        crate::util::text::truncate_on_char_boundary(s, max_bytes).trim_end()
    )
}

/// Map an `AggregatedHookResult` from a PrePermission hook chain into a
/// concrete decision. Deny wins over Allow when both signals are present
/// in the same chain (safer default for ambiguous configs).
pub fn classify_pre_permission(res: &super::hooks::AggregatedHookResult) -> PrePermissionDecision {
    if res.blocked || res.abort {
        return PrePermissionDecision::Deny;
    }
    if res.allow {
        return PrePermissionDecision::Allow;
    }
    PrePermissionDecision::Passthrough
}

/// Abstract permission checker — injected by the engine so RunTools doesn't
/// need to know about PermissionManager internals.
#[async_trait]
pub trait PermissionChecker: Send + Sync {
    /// Returns `Ok(true)` if tool execution is allowed, `Ok(false)` if denied,
    /// or `Err(...)` if the check itself failed.
    ///
    /// `Err(...)` is treated as a denial — the pipeline is fail-closed. An
    /// implementation that cannot reach a decision must not expect the tool to
    /// run; return `Ok(true)` explicitly if that is what you mean.
    async fn check(
        &self,
        tool: &dyn Tool,
        input: &serde_json::Value,
        cancel: &CancellationToken,
        agent_id: &str,
    ) -> Result<bool>;
}

/// A no-op checker that allows everything (used when permissions are disabled).
pub struct AllowAllPermissions;

#[async_trait]
impl PermissionChecker for AllowAllPermissions {
    async fn check(
        &self,
        _tool: &dyn Tool,
        _input: &serde_json::Value,
        _cancel: &CancellationToken,
        _agent_id: &str,
    ) -> Result<bool> {
        Ok(true)
    }
}

/// Context passed through the tool execution pipeline.
pub struct RunContext<'a> {
    pub agent_id: &'a str,
    pub working_dir: &'a str,
    pub agent_data_dir: &'a str,
    pub tools: &'a [Arc<dyn Tool>],
    /// Re-fetch the live tool list (includes ToolSearch discoveries). When
    /// set, serial execution refreshes after `ToolSearch` so deferred tools
    /// can be called in the same assistant turn.
    pub tools_resolver: Option<&'a (dyn Fn() -> Vec<Arc<dyn Tool>> + Send + Sync)>,
    /// Fire an engine event (provided by the engine for callback emission).
    pub fire: &'a (dyn Fn(EngineEvent) + Send + Sync),
    /// Permission checker instance.
    pub permission_checker: &'a dyn PermissionChecker,
    /// Event bus for tools that need it (AskUser, etc.).
    pub event_bus: Option<&'a EventBus>,
    /// Response registry for tools that need request-response (AskUser, etc.).
    pub response_registry: Option<&'a ResponseRegistry>,
    /// Hook manager for PreToolUse / PostToolUse hooks (optional).
    pub hook_manager: Option<Arc<HookManager>>,
    /// HTTP client passed to prompt hooks.
    pub hook_client: Option<reqwest::Client>,
    /// Model profile passed to prompt hooks.
    pub hook_profile: Option<ModelProfile>,
    /// Session id for hook base payload.
    pub session_id: String,
}

// ============================================================================
// Public entry points
// ============================================================================

/// Execute a list of tool_use blocks from an assistant message.
/// Read-only tools run concurrently; write tools run serially.
pub async fn run_tools(
    tool_uses: &[ContentBlock],
    cancel: &CancellationToken,
    ctx: &RunContext<'_>,
) -> Vec<ContentBlock> {
    // Determine if all tools are read-only. Classify via the same resolver
    // dispatch uses so a configured alias/override (Plugins → Alias) is judged
    // by the tool that will actually EXECUTE — overriding a read-only tool
    // with a mutating one must not slip into the concurrent path.
    let all_read_only = tool_uses.iter().all(|tu| {
        if let ContentBlock::ToolUse { name, .. } = tu {
            crate::tools::tool_search::resolve_tool_by_name(name, ctx.tools)
                .map(|t| t.is_read_only())
                .unwrap_or(false)
        } else {
            false
        }
    });

    if all_read_only && tool_uses.len() > 1 {
        run_concurrently(tool_uses, cancel, ctx).await
    } else {
        run_serially(tool_uses, cancel, ctx).await
    }
}

async fn run_concurrently(
    tool_uses: &[ContentBlock],
    cancel: &CancellationToken,
    ctx: &RunContext<'_>,
) -> Vec<ContentBlock> {
    let futures: Vec<_> = tool_uses
        .iter()
        .map(|tu| run_single_tool(tu, cancel, ctx))
        .collect();

    let results = futures::future::join_all(futures).await;

    // Flatten — each future returns a Vec<ContentBlock> (typically 1)
    let mut output = Vec::new();
    for group in results {
        output.extend(group);
    }
    output
}

async fn run_serially(
    tool_uses: &[ContentBlock],
    cancel: &CancellationToken,
    ctx: &RunContext<'_>,
) -> Vec<ContentBlock> {
    let mut results = Vec::new();
    let mut active_tools: Vec<Arc<dyn Tool>> = ctx.tools.to_vec();
    for tu in tool_uses {
        if cancel.is_cancelled() {
            // Generate stop messages for remaining tools
            for remaining in tool_uses.iter().skip(results.len()) {
                if let ContentBlock::ToolUse { id, .. } = remaining {
                    results.push(create_tool_result_stop(id));
                }
            }
            break;
        }
        let dynamic_ctx = RunContext {
            agent_id: ctx.agent_id,
            working_dir: ctx.working_dir,
            agent_data_dir: ctx.agent_data_dir,
            tools: &active_tools,
            tools_resolver: ctx.tools_resolver,
            fire: ctx.fire,
            permission_checker: ctx.permission_checker,
            event_bus: ctx.event_bus,
            response_registry: ctx.response_registry,
            hook_manager: ctx.hook_manager.clone(),
            hook_client: ctx.hook_client.clone(),
            hook_profile: ctx.hook_profile.clone(),
            session_id: ctx.session_id.clone(),
        };
        results.extend(run_single_tool(tu, cancel, &dynamic_ctx).await);

        if let ContentBlock::ToolUse { name, .. } = tu {
            if name == "ToolSearch" {
                if let Some(resolver) = ctx.tools_resolver {
                    active_tools = resolver();
                }
            }
        }
    }
    results
}

// ============================================================================
// Single tool execution
// ============================================================================

async fn run_single_tool(
    tool_use: &ContentBlock,
    cancel: &CancellationToken,
    ctx: &RunContext<'_>,
) -> Vec<ContentBlock> {
    let (tool_name, tool_id, input) = match tool_use {
        ContentBlock::ToolUse { name, id, input } => (name.clone(), id.clone(), input.clone()),
        _ => return vec![],
    };

    // Find the tool — try active set, then refresh from resolver (ToolSearch
    // may have loaded deferred tools earlier in this serial batch).
    let tool =
        crate::tools::tool_search::resolve_tool_by_name(&tool_name, ctx.tools).or_else(|| {
            ctx.tools_resolver.map(|resolver| {
                let fresh = resolver();
                crate::tools::tool_search::resolve_tool_by_name(&tool_name, &fresh)
            })?
        });

    let tool = match tool {
        Some(t) => t,
        None => {
            let error_msg = format!("Error: No such tool available: {tool_name}. Please use the ToolSearch tool to find and load the correct tool before retrying.");
            warn!(
                "[RunTools] tool not found agent={} tool={tool_name} id={tool_id}",
                ctx.agent_id
            );
            (ctx.fire)(EngineEvent::ToolExecutionError(ToolExecutionErrorData {
                agent_id: ctx.agent_id.to_string(),
                tool_name: tool_name.clone(),
                title: tool_name.clone(),
                description: step_description(&input),
                args_shape: arg_shape(&input),
                content: error_msg.clone(),
            }));
            return vec![ContentBlock::ToolResult {
                tool_use_id: tool_id,
                content: error_msg,
                is_error: true,
            }];
        }
    };

    // Checkpoint: cancelled before starting
    if cancel.is_cancelled() {
        tracing::info!(
            "[RunTools] skipped cancelled tool agent={} tool={} id={}",
            ctx.agent_id,
            tool_name,
            tool_id
        );
        return vec![create_tool_result_stop(&tool_id)];
    }

    // Resolve the model's paths ONCE, here, before anything reads the input.
    //
    // A relative path used to be passed straight to `std::fs`, so it resolved
    // against the *daemon process* cwd — which on a desktop install is the app
    // bundle's `Contents/Resources` (the supervisor starts the daemon there).
    // `Read` then reported "File not found" for a file sitting in the project,
    // or worse succeeded against an unrelated file that happens to exist in
    // that directory, and `Write` created files inside the signed bundle.
    //
    // This is also the only place that works: `validate_input` borrows the
    // input and cannot rewrite it, and `gen_tool_permission` (the approval
    // card) gets no working directory — so resolving inside the tool would
    // leave the user approving one path while another was written.
    let input = resolve_path_inputs(tool.as_ref(), input, ctx.working_dir);

    // Validate input schema (basic JSON schema check)
    if let Err(validation_err) = validate_tool_input(&tool, &input) {
        (ctx.fire)(EngineEvent::ToolExecutionError(ToolExecutionErrorData {
            agent_id: ctx.agent_id.to_string(),
            tool_name: tool_name.clone(),
            title: tool.get_display_title(&input),
            description: step_description(&input),
            args_shape: arg_shape(&input),
            content: validation_err.clone(),
        }));
        return vec![ContentBlock::ToolResult {
            tool_use_id: tool_id,
            content: validation_err,
            is_error: true,
        }];
    }

    // Custom validate_input
    let tool_ctx = ToolContext {
        agent_id: ctx.agent_id,
        working_dir: ctx.working_dir,
        agent_data_dir: ctx.agent_data_dir,
        abort: cancel.clone(),
        event_bus: ctx.event_bus,
        response_registry: ctx.response_registry,
        edit_format: ctx
            .hook_profile
            .as_ref()
            .and_then(|p| p.edit_format)
            .unwrap_or_default(),
    };
    if let Err(validation_msg) = tool.validate_input(&input, &tool_ctx).await {
        (ctx.fire)(EngineEvent::ToolExecutionError(ToolExecutionErrorData {
            agent_id: ctx.agent_id.to_string(),
            tool_name: tool_name.clone(),
            title: tool.get_display_title(&input),
            description: step_description(&input),
            args_shape: arg_shape(&input),
            content: validation_msg.clone(),
        }));
        return vec![ContentBlock::ToolResult {
            tool_use_id: tool_id,
            content: validation_msg,
            is_error: true,
        }];
    }

    // PreToolUse hook — may block the tool or update its input
    let input = if let Some(ref hm) = ctx.hook_manager {
        if hm.has_hooks_for_event(&HookEvent::PreToolUse) {
            let base = HookInputBase {
                hook_event_name: HookEvent::PreToolUse,
                session_id: ctx.session_id.clone(),
                agent_id: ctx.agent_id.to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                cwd: ctx.working_dir.to_string(),
            };
            let hook_input = HookInput::PreToolUse(PreToolUseInput {
                base,
                tool_name: tool_name.clone(),
                tool_input: input.clone(),
            });
            let result = zen_hooks::execute_hooks(
                hm,
                &HookEvent::PreToolUse,
                &hook_input,
                &ExecuteHooksOptions {
                    client: ctx.hook_client.as_ref(),
                    profile: ctx.hook_profile.as_ref(),
                    ..Default::default()
                },
            )
            .await;

            if result.blocked {
                let reason = result.reason.unwrap_or_else(|| "Blocked by hook".into());
                (ctx.fire)(EngineEvent::ToolExecutionError(ToolExecutionErrorData {
                    agent_id: ctx.agent_id.to_string(),
                    tool_name: tool_name.clone(),
                    title: tool.get_display_title(&input),
                    description: step_description(&input),
                    args_shape: arg_shape(&input),
                    content: reason.clone(),
                }));
                return vec![ContentBlock::ToolResult {
                    tool_use_id: tool_id,
                    content: reason,
                    is_error: true,
                }];
            }

            // Hook may supply updated input — re-resolve, since a hook is
            // free to hand back a relative path and everything below this
            // point (permission card included) reads what it returns.
            match result.updated_input {
                Some(updated) => resolve_path_inputs(tool.as_ref(), updated, ctx.working_dir),
                None => input,
            }
        } else {
            input
        }
    } else {
        input
    };

    // Permission check for write tools
    if !tool.is_read_only() {
        if cancel.is_cancelled() {
            tracing::info!(
                "[RunTools] skipped cancelled write tool agent={} tool={} id={}",
                ctx.agent_id,
                tool_name,
                tool_id
            );
            return vec![create_tool_result_stop(&tool_id)];
        }

        // PrePermission hook — runs synchronously so it can short-circuit
        // the user prompt entirely. `decision: "allow"` skips the prompt
        // and grants the tool; blocked/`decision: "reject"` denies the
        // tool without bothering the user; otherwise we fall through to
        // the normal permission flow.
        let pre_perm_decision: PrePermissionDecision = if let Some(ref hm) = ctx.hook_manager {
            if hm.has_hooks_for_event(&HookEvent::PrePermission) {
                let base = HookInputBase {
                    hook_event_name: HookEvent::PrePermission,
                    session_id: ctx.session_id.clone(),
                    agent_id: ctx.agent_id.to_string(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    cwd: ctx.working_dir.to_string(),
                };
                let input_for_hook = HookInput::PrePermission(PrePermissionInput {
                    base,
                    tool_name: tool_name.clone(),
                    tool_input: input.clone(),
                });
                let res = zen_hooks::execute_hooks(
                    hm,
                    &HookEvent::PrePermission,
                    &input_for_hook,
                    &ExecuteHooksOptions {
                        client: ctx.hook_client.as_ref(),
                        profile: ctx.hook_profile.as_ref(),
                        ..Default::default()
                    },
                )
                .await;
                classify_pre_permission(&res)
            } else {
                PrePermissionDecision::Passthrough
            }
        } else {
            PrePermissionDecision::Passthrough
        };

        // Short-circuit on allow/deny; otherwise continue to the user prompt.
        let permission_result: Result<bool> = match pre_perm_decision {
            PrePermissionDecision::Allow => {
                tracing::info!(
                    "[RunTools] PrePermission hook allowed tool={} id={}",
                    tool_name,
                    tool_id
                );
                Ok(true)
            }
            PrePermissionDecision::Deny => {
                tracing::warn!(
                    "[RunTools] PrePermission hook denied tool={} id={}",
                    tool_name,
                    tool_id
                );
                Ok(false)
            }
            PrePermissionDecision::Passthrough => {
                tracing::info!(
                    "[RunTools] permission check agent={} tool={} id={}",
                    ctx.agent_id,
                    tool_name,
                    tool_id
                );
                ctx.permission_checker
                    .check(tool.as_ref(), &input, cancel, ctx.agent_id)
                    .await
            }
        };
        match permission_result {
            Ok(true) => {
                // Permission granted — proceed
                tracing::info!(
                    "[RunTools] permission granted agent={} tool={} id={}",
                    ctx.agent_id,
                    tool_name,
                    tool_id
                );

                // Fire PermissionRequest hook
                if let Some(ref hm) = ctx.hook_manager {
                    if hm.has_hooks_for_event(&HookEvent::PermissionRequest) {
                        let base = HookInputBase {
                            hook_event_name: HookEvent::PermissionRequest,
                            session_id: ctx.session_id.clone(),
                            agent_id: ctx.agent_id.to_string(),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            cwd: ctx.working_dir.to_string(),
                        };
                        let hook_input = HookInput::PermissionRequest(PermissionRequestInput {
                            base,
                            tool_name: tool_name.clone(),
                            tool_input: input.clone(),
                        });
                        let (client, profile) = (ctx.hook_client.clone(), ctx.hook_profile.clone());
                        let hm_clone = hm.clone();
                        let usage_bus = ctx.event_bus.cloned();
                        tokio::spawn(async move {
                            let _ = zen_hooks::execute_hooks(
                                &hm_clone,
                                &HookEvent::PermissionRequest,
                                &hook_input,
                                &ExecuteHooksOptions {
                                    env: std::collections::HashMap::new(),
                                    cancel: None,
                                    client: client.as_ref(),
                                    profile: profile.as_ref(),
                                    messages: None,
                                    usage_bus: usage_bus.as_ref(),
                                },
                            )
                            .await;
                        });
                    }
                }
            }
            Ok(false) => {
                // Permission denied
                tracing::warn!(
                    "[RunTools] permission denied agent={} tool={} id={}",
                    ctx.agent_id,
                    tool_name,
                    tool_id
                );
                let msg = "Tool execution was cancelled by user.".to_string();
                return vec![ContentBlock::ToolResult {
                    tool_use_id: tool_id,
                    content: msg,
                    is_error: true,
                }];
            }
            Err(e) => {
                // Fail closed. A checker that cannot answer is not a checker
                // that said yes — if the permission system is broken we must
                // deny, or any error path becomes a way to run Bash/Write/Edit
                // with no approval at all.
                warn!(
                    "[RunTools] permission check FAILED — tool denied agent={} tool={} id={} error={e:#}",
                    ctx.agent_id, tool_name, tool_id
                );
                return vec![ContentBlock::ToolResult {
                    tool_use_id: tool_id,
                    content: format!(
                        "Tool execution denied: the permission check could not be completed ({e:#})."
                    ),
                    is_error: true,
                }];
            }
        }
    }

    // Execute the tool
    tracing::info!(
        "[RunTools] start agent={} tool={} id={} read_only={}",
        ctx.agent_id,
        tool_name,
        tool_id,
        tool.is_read_only()
    );
    match tool.call(input.clone(), &tool_ctx).await {
        Ok(outputs) => {
            tracing::info!(
                "[RunTools] complete agent={} tool={} id={} outputs={}",
                ctx.agent_id,
                tool_name,
                tool_id,
                outputs.len()
            );
            let mut results = Vec::new();
            for output in outputs {
                match output {
                    ToolOutput::Progress { message } => {
                        debug!("[{tool_name}] progress: {message}");
                    }
                    ToolOutput::Result {
                        data,
                        result_for_assistant,
                    } => {
                        // OutputFilter hook — last chance to redact / truncate
                        // the structured tool output before it reaches the
                        // chat UI and the engine context.
                        let mut data = data;
                        let mut result_for_assistant = result_for_assistant;
                        if let Some(ref hm) = ctx.hook_manager {
                            if hm.has_hooks_for_event(&HookEvent::OutputFilter) {
                                let base = HookInputBase {
                                    hook_event_name: HookEvent::OutputFilter,
                                    session_id: ctx.session_id.clone(),
                                    agent_id: ctx.agent_id.to_string(),
                                    timestamp: chrono::Utc::now().to_rfc3339(),
                                    cwd: ctx.working_dir.to_string(),
                                };
                                let res = zen_hooks::execute_hooks(
                                    hm,
                                    &HookEvent::OutputFilter,
                                    &HookInput::OutputFilter(OutputFilterInput {
                                        base,
                                        tool_name: tool_name.clone(),
                                        tool_input: input.clone(),
                                        tool_output: data.clone(),
                                    }),
                                    &ExecuteHooksOptions {
                                        client: ctx.hook_client.as_ref(),
                                        profile: ctx.hook_profile.as_ref(),
                                        ..Default::default()
                                    },
                                )
                                .await;
                                if let Some(new_out) = res.updated_output {
                                    // Mirror the replacement into the
                                    // assistant-facing text too.
                                    if let Some(s) = new_out.as_str() {
                                        result_for_assistant = s.to_string();
                                    } else {
                                        result_for_assistant = serde_json::to_string(&new_out)
                                            .unwrap_or(result_for_assistant);
                                    }
                                    data = new_out;
                                }
                            }
                        }

                        // Language-server feedback: after a file-writing tool,
                        // push the file to the workspace's server and append
                        // its diagnostics, so the model sees a type error now
                        // rather than after it decides to run the build.
                        // Bounded wait; absent server → nothing appended.
                        if matches!(tool_name.as_str(), "Edit" | "Write" | "NotebookEdit") {
                            let written = data
                                .get("path")
                                .or_else(|| data.get("file_path"))
                                .and_then(|v| v.as_str())
                                .map(str::to_string);
                            if let Some(p) = written {
                                if let Some(report) =
                                    crate::lsp::diagnostics_after_write(ctx.working_dir, &p).await
                                {
                                    let block = report.render();
                                    if !block.is_empty() {
                                        result_for_assistant.push_str(&block);
                                    }
                                    if let serde_json::Value::Object(ref mut map) = data {
                                        map.insert(
                                            "lspDiagnostics".into(),
                                            serde_json::json!({
                                                "server": report.server,
                                                "fresh": report.fresh,
                                                "items": report.diagnostics,
                                            }),
                                        );
                                    }
                                }
                            }
                        }

                        // Emit tool:execution:complete
                        let msg = tool.gen_tool_result_message(&data, &input);
                        (ctx.fire)(EngineEvent::ToolExecutionComplete(
                            ToolExecutionCompleteData {
                                agent_id: ctx.agent_id.to_string(),
                                tool_name: tool_name.clone(),
                                title: msg.title,
                                summary: msg.summary,
                                description: step_description(&input),
                                content: msg.content,
                            },
                        ));

                        // PostToolUse hook (non-blockable, fire-and-forget semantics ok)
                        if let Some(ref hm) = ctx.hook_manager {
                            if hm.has_hooks_for_event(&HookEvent::PostToolUse) {
                                let base = HookInputBase {
                                    hook_event_name: HookEvent::PostToolUse,
                                    session_id: ctx.session_id.clone(),
                                    agent_id: ctx.agent_id.to_string(),
                                    timestamp: chrono::Utc::now().to_rfc3339(),
                                    cwd: ctx.working_dir.to_string(),
                                };
                                zen_hooks::execute_hooks(
                                    hm,
                                    &HookEvent::PostToolUse,
                                    &HookInput::PostToolUse(PostToolUseInput {
                                        base,
                                        tool_name: tool_name.clone(),
                                        tool_input: input.clone(),
                                        tool_response: data.clone(),
                                    }),
                                    &ExecuteHooksOptions {
                                        client: ctx.hook_client.as_ref(),
                                        profile: ctx.hook_profile.as_ref(),
                                        ..Default::default()
                                    },
                                )
                                .await;
                            }
                        }

                        // L1 offload (§8 "no silent edits"): behind
                        // `controlPlane.workspace.substituteToolOutput`
                        // (default off — docs/control-plane.md §4). The
                        // length check runs first and is in-memory only, so a
                        // typical small result never touches config.json.
                        if result_for_assistant.len() > crate::control_plane::workspace::OFFLOAD_THRESHOLD_BYTES {
                            let settings = crate::gateway::group_manager::load_control_plane_settings(
                                &crate::control_plane::default_config_path(),
                            );
                            if settings.workspace.substitute_tool_output {
                                let ws = crate::control_plane::workspace::Workspace::for_chat(ctx.agent_data_dir);
                                match ws.write_artifact(&tool_id, &result_for_assistant) {
                                    Ok(preview) => result_for_assistant = preview.preview,
                                    Err(e) => tracing::debug!(
                                        "[control-plane] L1 offload failed for {tool_name} {tool_id}: {e}"
                                    ),
                                }
                            }
                        }
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: tool_id.clone(),
                            content: result_for_assistant,
                            is_error: false,
                        });
                    }
                    ToolOutput::ClearContextAndStart {
                        plan_file_path,
                        plan_content,
                    } => {
                        (ctx.fire)(EngineEvent::ToolExecutionComplete(
                            ToolExecutionCompleteData {
                                agent_id: ctx.agent_id.to_string(),
                                tool_name: tool_name.clone(),
                                title: "ExitPlanMode".to_string(),
                                summary: "clearContextAndStart".to_string(),
                                description: String::new(),
                                content: serde_json::json!({
                                    "planFilePath": plan_file_path,
                                    "selected": "clearContextAndStart"
                                }),
                            },
                        ));
                        results.push(ContentBlock::ControlSignal {
                            signal_type: "ClearContextAndStart".to_string(),
                            payload: serde_json::json!({
                                "plan_file_path": plan_file_path,
                                "plan_content": plan_content
                            }),
                        });
                    }
                }
            }
            results
        }
        Err(e) => {
            tracing::warn!(
                "[RunTools] error agent={} tool={} id={}: {e}",
                ctx.agent_id,
                tool_name,
                tool_id
            );
            let error_msg = format!("Tool execution failed: {e}");
            (ctx.fire)(EngineEvent::ToolExecutionError(ToolExecutionErrorData {
                agent_id: ctx.agent_id.to_string(),
                tool_name: tool_name.clone(),
                title: tool.get_display_title(&input),
                description: step_description(&input),
                args_shape: arg_shape(&input),
                content: error_msg.clone(),
            }));
            vec![ContentBlock::ToolResult {
                tool_use_id: tool_id,
                content: error_msg,
                is_error: true,
            }]
        }
    }
}

// ============================================================================
// Input validation
// ============================================================================

/// Validate tool input against the tool's JSON Schema.
fn validate_tool_input(
    tool: &Arc<dyn Tool>,
    input: &serde_json::Value,
) -> std::result::Result<(), String> {
    let schema = tool.input_schema();

    // If schema is empty or just {}, skip validation
    if schema.is_null()
        || (schema.is_object() && schema.as_object().map_or(false, |o| o.is_empty()))
    {
        return Ok(());
    }

    // Use jsonschema crate for validation if available, otherwise basic check
    // For now: basic required-field check
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        for field in required {
            if let Some(field_name) = field.as_str() {
                if input.get(field_name).is_none()
                    || input.get(field_name) == Some(&serde_json::Value::Null)
                {
                    return Err(format!("Missing required field: {field_name}"));
                }
            }
        }
    }

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::hooks::AggregatedHookResult;

    fn empty_aggr() -> AggregatedHookResult {
        AggregatedHookResult::empty()
    }

    /// The seam that makes the approval card show the file that will actually
    /// be written. Field-name matching alone would be wrong here — an MCP
    /// tool's `path` can be a wiki page — so resolution follows each tool's
    /// own `path_fields` declaration.
    #[test]
    fn only_declared_path_fields_are_resolved() {
        use crate::tools::{BashTool, ReadTool};

        let out = resolve_path_inputs(
            &ReadTool,
            serde_json::json!({"file_path": "a.rs", "limit": 5}),
            "/work/proj",
        );
        assert_eq!(out["file_path"], "/work/proj/a.rs");
        assert_eq!(out["limit"], 5, "other fields are untouched");

        // Bash declares no path fields: a `path` on a tool that never asked
        // for resolution must survive verbatim.
        let out = resolve_path_inputs(
            &BashTool,
            serde_json::json!({"path": "docs/page"}),
            "/work/proj",
        );
        assert_eq!(out["path"], "docs/page");

        // Nothing to resolve against — a one-shot with no working dir.
        let out = resolve_path_inputs(&ReadTool, serde_json::json!({"file_path": "a.rs"}), "");
        assert_eq!(out["file_path"], "a.rs");

        // An absolute path is exactly what the model sent.
        let out = resolve_path_inputs(
            &ReadTool,
            serde_json::json!({"file_path": "/etc/hosts"}),
            "/work",
        );
        assert_eq!(out["file_path"], "/etc/hosts");
    }

    #[test]
    fn classify_pre_permission_no_signals_is_passthrough() {
        assert_eq!(
            classify_pre_permission(&empty_aggr()),
            PrePermissionDecision::Passthrough
        );
    }

    #[test]
    fn classify_pre_permission_allow_flag_is_allow() {
        let mut a = empty_aggr();
        a.allow = true;
        assert_eq!(classify_pre_permission(&a), PrePermissionDecision::Allow);
    }

    #[test]
    fn classify_pre_permission_blocked_overrides_allow() {
        let mut a = empty_aggr();
        a.allow = true;
        a.blocked = true;
        assert_eq!(classify_pre_permission(&a), PrePermissionDecision::Deny);
    }

    #[test]
    fn classify_pre_permission_abort_is_deny() {
        let mut a = empty_aggr();
        a.abort = true;
        assert_eq!(classify_pre_permission(&a), PrePermissionDecision::Deny);
    }

    struct TestReadTool;
    #[async_trait::async_trait]
    impl Tool for TestReadTool {
        fn name(&self) -> &str {
            "read"
        }
        fn description(&self) -> &str {
            "Read a file"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            })
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<Vec<ToolOutput>> {
            Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"content": "hello"}),
                result_for_assistant: "hello".into(),
            }])
        }
        fn gen_tool_result_message(
            &self,
            _data: &serde_json::Value,
            _input: &serde_json::Value,
        ) -> ToolResultMessage {
            ToolResultMessage {
                title: "Read".into(),
                summary: "Read file".into(),
                content: serde_json::json!({}),
            }
        }
        fn get_display_title(&self, _input: &serde_json::Value) -> String {
            "Read file".into()
        }
    }

    struct TestWriteTool;
    #[async_trait::async_trait]
    impl Tool for TestWriteTool {
        fn name(&self) -> &str {
            "write"
        }
        fn description(&self) -> &str {
            "Write a file"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            })
        }
        fn is_read_only(&self) -> bool {
            false
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<Vec<ToolOutput>> {
            Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"written": true}),
                result_for_assistant: "written".into(),
            }])
        }
        fn gen_tool_result_message(
            &self,
            _data: &serde_json::Value,
            _input: &serde_json::Value,
        ) -> ToolResultMessage {
            ToolResultMessage {
                title: "Write".into(),
                summary: "Wrote file".into(),
                content: serde_json::json!({}),
            }
        }
        fn get_display_title(&self, _input: &serde_json::Value) -> String {
            "Write file".into()
        }
    }

    fn test_ctx<'a>(
        tools: &'a [Arc<dyn Tool>],
        checker: &'a dyn PermissionChecker,
    ) -> RunContext<'a> {
        RunContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            tools,
            tools_resolver: None,
            fire: &|_| {},
            permission_checker: checker,
            event_bus: None,
            response_registry: None,
            hook_manager: None,
            hook_client: None,
            hook_profile: None,
            session_id: String::new(),
        }
    }

    struct BigResultTool;
    #[async_trait::async_trait]
    impl Tool for BigResultTool {
        fn name(&self) -> &str {
            "BigResultTool"
        }
        fn description(&self) -> &str {
            "returns a result bigger than the L1 offload threshold"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn call(&self, _input: serde_json::Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
            Ok(vec![ToolOutput::Result {
                data: serde_json::json!({}),
                result_for_assistant: "X".repeat(crate::control_plane::workspace::OFFLOAD_THRESHOLD_BYTES + 1),
            }])
        }
        fn gen_tool_result_message(&self, _data: &serde_json::Value, _input: &serde_json::Value) -> ToolResultMessage {
            ToolResultMessage {
                title: "BigResultTool".into(),
                summary: String::new(),
                content: serde_json::json!({}),
            }
        }
        fn get_display_title(&self, _input: &serde_json::Value) -> String {
            "BigResultTool".into()
        }
    }

    /// L1 offload (§8), tested both ways per the lead's instruction. Isolated
    /// with `control_plane::env_test_guard` since `SENCLAW_CONFIG_PATH` /
    /// `SENCLAW_CONTROL_PLANE_WORKSPACE_DIR` are process-global and other
    /// modules' tests touch the same names.
    #[tokio::test]
    async fn l1_offload_only_replaces_a_large_result_when_the_switch_is_on() {
        let _guard = crate::control_plane::env_test_guard();
        let cfg_dir = tempfile::tempdir().unwrap();
        let config_path = cfg_dir.path().join("config.json");
        let ws_dir = tempfile::tempdir().unwrap();
        std::env::set_var("SENCLAW_CONFIG_PATH", &config_path);
        std::env::set_var("SENCLAW_CONTROL_PLANE_WORKSPACE_DIR", ws_dir.path());

        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(BigResultTool)];
        let checker = AllowAllPermissions;
        let ctx = test_ctx(&tools, &checker);
        let tool_uses = vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "BigResultTool".into(),
            input: serde_json::json!({}),
        }];
        let cancel = CancellationToken::new();

        // Off (default settings — no config.json written yet): untouched.
        let results = run_tools(&tool_uses, &cancel, &ctx).await;
        let ContentBlock::ToolResult { content, .. } = &results[0] else {
            panic!("expected a ToolResult block");
        };
        assert_eq!(content.len(), crate::control_plane::workspace::OFFLOAD_THRESHOLD_BYTES + 1, "off: not substituted");

        // On: substituted with a head+tail preview naming the cut and the path.
        let mut settings = crate::control_plane::ControlPlaneSettings::default();
        settings.workspace.substitute_tool_output = true;
        crate::gateway::group_manager::save_control_plane_settings(&config_path, &settings).unwrap();

        let tool_uses = vec![ContentBlock::ToolUse {
            id: "c2".into(),
            name: "BigResultTool".into(),
            input: serde_json::json!({}),
        }];
        let results = run_tools(&tool_uses, &cancel, &ctx).await;
        let ContentBlock::ToolResult { content, .. } = &results[0] else {
            panic!("expected a ToolResult block");
        };
        assert!(content.len() < crate::control_plane::workspace::OFFLOAD_THRESHOLD_BYTES, "on: substituted with a preview");
        assert!(content.contains("full output saved at"));
        let saved = std::fs::read_to_string(ws_dir.path().join(crate::control_plane::safe_id("/tmp")).join("artifacts").join("c2")).unwrap();
        assert_eq!(saved.len(), crate::control_plane::workspace::OFFLOAD_THRESHOLD_BYTES + 1, "the full result is still on disk");

        std::env::remove_var("SENCLAW_CONFIG_PATH");
        std::env::remove_var("SENCLAW_CONTROL_PLANE_WORKSPACE_DIR");
    }

    /// Ordering is the whole reason resolution lives here and not in the
    /// tools: `validate_input` only borrows the input, and the permission
    /// layer decides before `call` ever runs. A unit test of
    /// `resolve_path_inputs` alone proves none of that, so this drives the
    /// real entry point and records what each stage was handed.
    #[tokio::test]
    async fn validation_permission_and_call_all_see_the_resolved_path() {
        #[derive(Clone, Default)]
        struct Seen(Arc<std::sync::Mutex<Vec<(&'static str, String)>>>);
        impl Seen {
            fn note(&self, stage: &'static str, input: &serde_json::Value) {
                let p = input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.0.lock().unwrap().push((stage, p));
            }
            fn at(&self, stage: &str) -> String {
                self.0
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(s, _)| *s == stage)
                    .map(|(_, p)| p.clone())
                    .unwrap_or_else(|| panic!("stage {stage} never ran"))
            }
        }

        struct PathTool(Seen);
        #[async_trait::async_trait]
        impl Tool for PathTool {
            fn name(&self) -> &str {
                "PathTool"
            }
            fn description(&self) -> &str {
                "takes a path"
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": {"file_path": {"type": "string"}},
                    "required": ["file_path"]
                })
            }
            fn is_read_only(&self) -> bool {
                false // so the permission stage actually runs
            }
            fn path_fields(&self) -> &'static [&'static str] {
                &["file_path"]
            }
            async fn validate_input(
                &self,
                input: &serde_json::Value,
                _ctx: &ToolContext<'_>,
            ) -> std::result::Result<(), String> {
                self.0.note("validate", input);
                Ok(())
            }
            async fn call(
                &self,
                input: serde_json::Value,
                _ctx: &ToolContext<'_>,
            ) -> Result<Vec<ToolOutput>> {
                self.0.note("call", &input);
                Ok(vec![ToolOutput::Result {
                    data: serde_json::json!({}),
                    result_for_assistant: "ok".into(),
                }])
            }
            fn gen_tool_result_message(
                &self,
                _data: &serde_json::Value,
                _input: &serde_json::Value,
            ) -> ToolResultMessage {
                ToolResultMessage {
                    title: "PathTool".into(),
                    summary: String::new(),
                    content: serde_json::json!({}),
                }
            }
            fn get_display_title(&self, _input: &serde_json::Value) -> String {
                "PathTool".into()
            }
        }

        /// Stands in for the real checker, which is where the approval card is
        /// built (`gen_tool_permission` gets this same input).
        struct RecordingPermissions(Seen);
        #[async_trait::async_trait]
        impl PermissionChecker for RecordingPermissions {
            async fn check(
                &self,
                _tool: &dyn Tool,
                input: &serde_json::Value,
                _cancel: &CancellationToken,
                _agent_id: &str,
            ) -> Result<bool> {
                self.0.note("permission", input);
                Ok(true)
            }
        }

        let seen = Seen::default();
        let tool: Arc<dyn Tool> = Arc::new(PathTool(seen.clone()));
        let tools = vec![tool];
        let checker = RecordingPermissions(seen.clone());
        let ctx = test_ctx(&tools, &checker);
        let cancel = CancellationToken::new();

        run_single_tool(
            &ContentBlock::ToolUse {
                id: "tu-1".into(),
                name: "PathTool".into(),
                input: serde_json::json!({"file_path": "notes/today.md"}),
            },
            &cancel,
            &ctx,
        )
        .await;

        // test_ctx's working_dir is /tmp.
        for stage in ["validate", "permission", "call"] {
            assert_eq!(
                seen.at(stage),
                "/tmp/notes/today.md",
                "stage `{stage}` saw an unresolved path"
            );
        }
    }

    #[tokio::test]
    async fn run_readonly_tool_succeeds() {
        let tool: Arc<dyn Tool> = Arc::new(TestReadTool);
        let tools = vec![tool];
        let ctx = test_ctx(&tools, &AllowAllPermissions);
        let cancel = CancellationToken::new();

        let results = run_single_tool(
            &ContentBlock::ToolUse {
                id: "tu-1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "/tmp/test"}),
            },
            &cancel,
            &ctx,
        )
        .await;

        assert_eq!(results.len(), 1);
        if let ContentBlock::ToolResult {
            content, is_error, ..
        } = &results[0]
        {
            assert!(!is_error);
            assert_eq!(content, "hello");
        } else {
            panic!("Expected ToolResult");
        }
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let tools: Vec<Arc<dyn Tool>> = vec![];
        let ctx = test_ctx(&tools, &AllowAllPermissions);
        let cancel = CancellationToken::new();

        let results = run_single_tool(
            &ContentBlock::ToolUse {
                id: "tu-1".into(),
                name: "nonexistent".into(),
                input: serde_json::json!({}),
            },
            &cancel,
            &ctx,
        )
        .await;

        assert_eq!(results.len(), 1);
        if let ContentBlock::ToolResult {
            content, is_error, ..
        } = &results[0]
        {
            assert!(*is_error);
            assert!(content.contains("No such tool"));
        } else {
            panic!("Expected ToolResult");
        }
    }

    #[tokio::test]
    async fn validation_fails_on_missing_required() {
        let tool: Arc<dyn Tool> = Arc::new(TestReadTool);
        let tools = vec![tool.clone()];
        let ctx = test_ctx(&tools, &AllowAllPermissions);
        let cancel = CancellationToken::new();

        let results = run_single_tool(
            &ContentBlock::ToolUse {
                id: "tu-1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
            &cancel,
            &ctx,
        )
        .await;

        assert_eq!(results.len(), 1);
        if let ContentBlock::ToolResult { is_error, .. } = &results[0] {
            assert!(*is_error);
        } else {
            panic!("Expected ToolResult error");
        }
    }

    #[tokio::test]
    async fn cancelled_before_tool_returns_stop() {
        let tool: Arc<dyn Tool> = Arc::new(TestReadTool);
        let tools = vec![tool];
        let ctx = test_ctx(&tools, &AllowAllPermissions);
        let cancel = CancellationToken::new();
        cancel.cancel(); // Cancel immediately

        let results = run_single_tool(
            &ContentBlock::ToolUse {
                id: "tu-1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "/tmp/test"}),
            },
            &cancel,
            &ctx,
        )
        .await;

        assert_eq!(results.len(), 1);
        if let ContentBlock::ToolResult { content, .. } = &results[0] {
            assert!(content.contains("interrupted"));
        } else {
            panic!("Expected ToolResult stop");
        }
    }

    /// A checker whose check always errors — stands in for a broken/unreachable
    /// permission backend.
    struct FailingPermissions;

    #[async_trait::async_trait]
    impl PermissionChecker for FailingPermissions {
        async fn check(
            &self,
            _tool: &dyn Tool,
            _input: &serde_json::Value,
            _cancel: &CancellationToken,
            _agent_id: &str,
        ) -> Result<bool> {
            Err(anyhow::anyhow!("permission backend unavailable"))
        }
    }

    /// The permission pipeline is fail-closed: when the checker itself errors,
    /// the write tool must be denied rather than executed. A regression here
    /// would let any error path run Bash/Write/Edit with no approval.
    #[tokio::test]
    async fn permission_check_error_denies_write_tool() {
        let tool: Arc<dyn Tool> = Arc::new(TestWriteTool);
        let tools = vec![tool];
        let ctx = test_ctx(&tools, &FailingPermissions);
        let cancel = CancellationToken::new();

        let results = run_single_tool(
            &ContentBlock::ToolUse {
                id: "tu-1".into(),
                name: "write".into(),
                input: serde_json::json!({"path": "/tmp/test", "content": "x"}),
            },
            &cancel,
            &ctx,
        )
        .await;

        assert_eq!(results.len(), 1);
        if let ContentBlock::ToolResult {
            content, is_error, ..
        } = &results[0]
        {
            assert!(*is_error, "denial must be reported as an error");
            assert!(
                content.contains("denied"),
                "expected a denial message, got: {content}"
            );
            // The tool's own success output must never appear — proof that
            // `call` was not reached.
            assert!(
                !content.contains("written"),
                "tool executed despite a failed permission check: {content}"
            );
        } else {
            panic!("Expected ToolResult denial");
        }
    }

    #[test]
    fn a_steps_headline_is_the_models_own_description() {
        let input = serde_json::json!({
            "command": "grep -rn foo src/",
            "description": "Survey existing skills and the wiki capability",
        });
        assert_eq!(
            step_description(&input),
            "Survey existing skills and the wiki capability"
        );
    }

    #[test]
    fn a_tool_without_a_description_gets_no_invented_one() {
        // Read takes no `description`; an empty string is what tells the
        // clients to show the tool's verb instead of a guess.
        let input = serde_json::json!({"file_path": "/tmp/a.rs"});
        assert_eq!(step_description(&input), "");
        // Whitespace-only is the same as absent.
        assert_eq!(step_description(&serde_json::json!({"description": "   "})), "");
        // A non-string must not panic or stringify into the headline.
        assert_eq!(step_description(&serde_json::json!({"description": 7})), "");
    }

    #[test]
    fn a_long_description_is_cut_on_a_char_boundary() {
        // Multi-byte on purpose: a naive byte slice panics here.
        let long = "Khảo".repeat(200);
        let out = step_description(&serde_json::json!({"description": long}));
        assert!(out.len() <= 240, "len was {}", out.len());
        assert!(!out.is_empty());
        // Still valid UTF-8 text (it is a String, so the cut landed on a
        // boundary rather than panicking).
        assert!(out.starts_with("Khảo"));
    }

    #[test]
    fn an_argument_hint_says_what_the_call_was_for() {
        assert_eq!(
            argument_hint(&serde_json::json!({"query": "giá vàng hôm nay", "num_results": 5})),
            Some("giá vàng hôm nay".to_string())
        );
        // Preference order: a URL identifies a navigation better than its tab.
        assert_eq!(
            argument_hint(&serde_json::json!({"tab_id": "152675", "url": "https://laodong.vn/x"})),
            Some("https://laodong.vn/x".to_string())
        );
        // A number is a label; a structure is not.
        assert_eq!(
            argument_hint(&serde_json::json!({"id": 27})),
            Some("27".to_string())
        );
        assert_eq!(argument_hint(&serde_json::json!({"query": {"a": 1}})), None);
        assert_eq!(argument_hint(&serde_json::json!({"files": ["a", "b"]})), None);
        // Nothing recognised, and nothing invented.
        assert_eq!(argument_hint(&serde_json::json!({"engine": "google"})), None);
        assert_eq!(argument_hint(&serde_json::json!("not an object")), None);
    }

    #[test]
    fn a_result_excerpt_unwraps_the_mcp_envelope_instead_of_printing_it() {
        // The envelope is JSON wrapping JSON. Printing it raw is the failure
        // this replaces, so the excerpt must be the inner sentence.
        let data = serde_json::json!({
            "content": [{"type": "text", "text": "Đoán tỷ số\nLịch\nĐăng nhập\nTIN TỨC"}]
        });
        assert_eq!(
            result_excerpt(&data),
            Some("Đoán tỷ số Lịch Đăng nhập TIN TỨC".to_string())
        );
        // Still structured after unwrapping: no sentence to show.
        let structured = serde_json::json!({
            "content": [{"type": "text", "text": "{\"agent_id\": \"web:main\"}"}]
        });
        assert_eq!(result_excerpt(&structured), None);
        assert_eq!(result_excerpt(&serde_json::json!({"ok": true})), None);
    }

    #[test]
    fn a_long_hint_is_clipped_on_a_char_boundary() {
        let long = "Khảo sát ".repeat(40);
        let out = argument_hint(&serde_json::json!({"query": long})).unwrap();
        assert!(out.len() <= 124, "len {}", out.len());
        assert!(out.ends_with('…'));
        assert!(out.starts_with("Khảo sát"));
    }
}

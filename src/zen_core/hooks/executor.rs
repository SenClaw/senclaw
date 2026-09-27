//! Top-level hook execution pipeline.
//!
//! Port of TS `hooks/executeHooks.ts`.

use std::collections::HashMap;

use reqwest::Client;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::zen_core::ModelProfile;

use super::{
    command_executor::execute_command_hook,
    manager::HookManager,
    prompt_executor::execute_prompt_hook,
    types::{AggregatedHookResult, HookDefinition, HookError, HookEvent, HookInput, HookOutput},
};

/// Options for a single `execute_hooks` call.
pub struct ExecuteHooksOptions<'a> {
    pub env: HashMap<String, String>,
    pub cancel: Option<&'a CancellationToken>,
    /// HTTP client + profile for prompt hooks.
    pub client: Option<&'a Client>,
    pub profile: Option<&'a ModelProfile>,
    /// Message history for hooks with include_history enabled.
    pub messages: Option<&'a [crate::zen_core::Message]>,
    /// Engine bus for token accounting: when set, prompt-hook LLM calls emit
    /// `EngineEvent::LlmUsage` (source "hook") so hook spend is not invisible.
    pub usage_bus: Option<&'a crate::zen_core::EventBus>,
}

impl Default for ExecuteHooksOptions<'_> {
    fn default() -> Self {
        Self {
            env: HashMap::new(),
            cancel: None,
            client: None,
            profile: None,
            messages: None,
            usage_bus: None,
        }
    }
}

/// Execute all hooks matching `event` + `input`.
///
/// Async hooks are fired-and-forgotten.
/// Sync hooks run in parallel; their results are aggregated.
/// Returns an `AggregatedHookResult` indicating whether to block/abort the action.
pub async fn execute_hooks(
    hook_manager: &HookManager,
    event: &HookEvent,
    input: &HookInput,
    opts: &ExecuteHooksOptions<'_>,
) -> AggregatedHookResult {
    let raw_hooks = hook_manager.get_matching_hooks(event, input);
    if raw_hooks.is_empty() {
        return AggregatedHookResult::empty();
    }

    let hooks: Vec<HookDefinition> = raw_hooks.into_iter().map(|h| h.normalize()).collect();

    info!(
        "[hooks] Executing {} hook(s) for event {:?}{}",
        hooks.len(),
        event,
        input
            .match_query()
            .map_or(String::new(), |q| format!(" (match: {q})")),
    );

    // Serialize input once
    let input_json = serde_json::to_string(input).unwrap_or_else(|_| "{}".into());

    let (sync_hooks, async_hooks): (Vec<_>, Vec<_>) =
        hooks.into_iter().partition(|h| !h.is_fire_and_forget());

    // Fire-and-forget async hooks
    for hook in async_hooks {
        let input_json = input_json.clone();
        let env = opts.env.clone();
        let client_clone = opts.client.cloned();
        let profile_clone = opts.profile.cloned();
        let messages_clone = opts.messages.map(|m| m.to_vec());
        let bus_clone = opts.usage_bus.cloned();
        tokio::spawn(async move {
            let result = run_one_hook(
                &hook,
                &input_json,
                &env,
                None,
                client_clone.as_ref(),
                profile_clone.as_ref(),
                messages_clone.as_deref(),
                bus_clone.as_ref(),
            )
            .await;
            info!("[hooks] Async hook completed: {result:?}");
        });
    }

    // Run sync hooks in parallel
    let futures: Vec<_> = sync_hooks
        .iter()
        .map(|hook| {
            run_one_hook(
                hook,
                &input_json,
                &opts.env,
                opts.cancel,
                opts.client,
                opts.profile,
                opts.messages,
                opts.usage_bus,
            )
        })
        .collect();

    let settled = futures::future::join_all(futures).await;

    // Pair results with their hook definitions
    let mut results: Vec<(&HookDefinition, HookOutput)> = Vec::new();
    let mut errors: Vec<HookError> = Vec::new();

    for (hook, outcome) in sync_hooks.iter().zip(settled) {
        match outcome {
            Ok(output) => {
                info!("[hooks] Hook result: {:?}", output.decision);
                results.push((hook, output));
            }
            Err(e) => {
                let msg = e.to_string();
                info!("[hooks] Hook error: {msg}");
                errors.push(HookError {
                    hook: hook.clone(),
                    error: msg,
                });
            }
        }
    }

    aggregate_results(results, errors, event)
}

#[allow(clippy::too_many_arguments)]
async fn run_one_hook(
    hook: &HookDefinition,
    input_json: &str,
    env: &HashMap<String, String>,
    cancel: Option<&CancellationToken>,
    client: Option<&Client>,
    profile: Option<&ModelProfile>,
    messages: Option<&[crate::zen_core::Message]>,
    usage_bus: Option<&crate::zen_core::EventBus>,
) -> anyhow::Result<HookOutput> {
    use super::types::HookType;
    match hook.hook_type {
        HookType::Command => execute_command_hook(hook, input_json, env, cancel, messages).await,
        HookType::Prompt => {
            let client = client.ok_or_else(|| anyhow::anyhow!("No HTTP client for prompt hook"))?;
            let profile =
                profile.ok_or_else(|| anyhow::anyhow!("No model profile for prompt hook"))?;
            execute_prompt_hook(hook, input_json, client, profile, cancel, messages, usage_bus)
                .await
        }
    }
}

fn aggregate_results(
    results: Vec<(&HookDefinition, HookOutput)>,
    errors: Vec<HookError>,
    event: &HookEvent,
) -> AggregatedHookResult {
    let non_blockable = event.is_non_blockable();

    let blocked = !non_blockable
        && results
            .iter()
            .any(|(hook, out)| hook.is_blocking() && out.is_blocked());

    let abort = blocked
        && results
            .iter()
            .any(|(hook, out)| hook.is_blocking() && out.is_abort());

    let reason = results
        .iter()
        .find(|(hook, out)| hook.is_blocking() && out.reason.is_some())
        .and_then(|(_, out)| out.reason.clone());

    let updated_input = results
        .iter()
        .filter(|(_, out)| out.updated_input.is_some())
        .last()
        .and_then(|(_, out)| out.updated_input.clone());

    // Last OutputFilter hook to set `updatedOutput` wins — same convention
    // as `updated_input`.
    let updated_output = results
        .iter()
        .filter(|(_, out)| out.updated_output.is_some())
        .last()
        .and_then(|(_, out)| out.updated_output.clone());

    // Any explicit `decision: "allow"` from a PrePermission hook grants
    // the tool. Deny (blocked) still wins because the aggregate above
    // already short-circuits via `blocked`.
    let allow = results.iter().any(|(_, out)| out.is_allow());

    let additional_context: Vec<String> = results
        .iter()
        .filter_map(|(_, out)| out.additional_context.clone())
        .collect();

    AggregatedHookResult {
        blocked,
        abort,
        reason,
        updated_input,
        updated_output,
        allow,
        additional_context,
        errors,
    }
}

//! LLM query layer — routes to OpenAI or Anthropic adapters.
//!
//! Each adapter streams the response via SSE, accumulates content blocks,
//! and returns a complete assistant [`Message`].
//!
//! Port of TS `queryLLM.ts` + `adapt/openai.ts` + `adapt/anthropic.ts`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::*;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a response may go **silent** before the connection is considered
/// dead. Unlike a total deadline this resets on every byte received, so it
/// bounds a stalled provider without bounding a long generation.
pub(crate) const STREAM_STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// The total-deadline timeout for one LLM request, or `None` for no deadline.
///
/// [`REQUEST_TIMEOUT`] exists to bound a remote provider that has stopped
/// answering. But a total deadline is *also* a ceiling on how long a generation
/// may legitimately take, and a local engine blows through it honestly:
/// `DEFAULT_MLX_MAX_NEW_TOKENS` is 8192, which at ~60 tok/s is over two minutes
/// of steady output — before prefill, and before a cold weight load. Cutting
/// that off looks exactly like the model dying mid-sentence.
///
/// So a loopback endpoint gets no deadline. Nothing is lost: a local engine
/// that hangs is caught by the client's `read_timeout`
/// ([`STREAM_STALL_TIMEOUT`]), which resets on every byte — silence kills, a
/// slow-but-moving stream does not. This covers Ollama and LM Studio as much as
/// it covers a SenClaw local model.
fn total_request_timeout(profile: &ModelProfile) -> Option<Duration> {
    if is_loopback_endpoint(&profile.base_url) {
        None
    } else {
        Some(REQUEST_TIMEOUT)
    }
}

/// Does this base URL point at this machine?
///
/// Hand-rolled rather than pulled through a URL parser because the answer must
/// be conservative in one specific direction: `http://127.0.0.1.evil.com/` is
/// **not** loopback, and a prefix test would say it is. The host is isolated
/// first (scheme, userinfo, port, path all stripped), then matched whole.
fn is_loopback_endpoint(base_url: &str) -> bool {
    let rest = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // `user:pass@host` — the last `@` wins, since userinfo may contain one.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);

    let host = if let Some(after) = authority.strip_prefix('[') {
        // IPv6 literal: `[::1]:8080`. No closing bracket means malformed, and
        // malformed is not loopback.
        match after.split_once(']') {
            Some((h, _)) => h,
            None => return false,
        }
    } else {
        authority.split(':').next().unwrap_or("")
    };
    let host = host.trim().to_ascii_lowercase();

    // RFC 6761: `localhost` and anything under `.localhost` resolve to loopback.
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if host == "::1" || host == "0:0:0:0:0:0:0:1" {
        return true;
    }
    // The whole 127.0.0.0/8 block, not just 127.0.0.1 — all four labels must be
    // numeric, so `127.0.0.1.evil.com` (five labels) is rejected.
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() == 4
        && labels[0] == "127"
        && labels
            .iter()
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()))
}

/// Sink for assistant text as it streams off the provider — called once per
/// text delta, in arrival order, while the turn is still running.
///
/// `conversation.rs` wires it to `EngineEvent::TextChunk`, which the AgentPool
/// forwards to the UI as `agent:delta`. That is what lets a client start on the
/// first sentence (voice chat speaks it through TTS) instead of waiting for the
/// completed turn.
pub type TextDeltaSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Reassembles SSE lines across network chunk boundaries.
///
/// A `bytes_stream()` chunk cuts wherever TCP happened to split: mid-line, and
/// mid-UTF-8-sequence. Decoding each chunk on its own and iterating `.lines()`
/// therefore handed truncated JSON to the parser, which `continue`d past it —
/// silently dropping whatever text that event carried. Buffer bytes, cut on
/// `\n`, and only decode complete lines.
#[derive(Default)]
struct SseLines {
    buf: Vec<u8>,
}

impl SseLines {
    /// Feed one network chunk; returns every complete line it completed.
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            out.push(String::from_utf8_lossy(&line).trim().to_string());
        }
        out
    }

    /// Whatever is left when the stream ends without a trailing newline.
    fn flush(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let line = String::from_utf8_lossy(&self.buf).trim().to_string();
        self.buf.clear();
        (!line.is_empty()).then_some(line)
    }
}

// ============================================================================
// Credentials
// ============================================================================

/// Attach the profile's credential to a request.
///
/// Two shapes: a plain API key in the provider's own header, or an OAuth
/// bearer token plus whatever protocol headers that provider needs to accept
/// one (see [`crate::providers::oauth::transport`]).
fn apply_auth(
    request: reqwest::RequestBuilder,
    profile: &ModelProfile,
    token: &str,
) -> reqwest::RequestBuilder {
    if let Some(provider_id) = profile.oauth_provider.as_deref() {
        let mut request = request;
        for (name, value) in crate::providers::oauth::transport::auth_headers(provider_id, token) {
            request = request.header(name, value);
        }
        return request;
    }

    // API-key path, unchanged: Anthropic wants `x-api-key`, everyone else a
    // bearer token.
    match effective_adapter(profile) {
        "anthropic" => request.header("x-api-key", token).header(
            "anthropic-version",
            crate::providers::oauth::transport::ANTHROPIC_VERSION,
        ),
        _ => request.header("Authorization", format!("Bearer {token}")),
    }
}

/// POST `body` to `url` with the profile's credential, refreshing once if the
/// provider says the token is no longer good.
///
/// The retry only fires for OAuth profiles: an API key that returns 401 is
/// wrong, not stale, and retrying it just doubles the latency of a hard error.
pub(crate) async fn post_authed(
    client: &Client,
    url: &str,
    profile: &ModelProfile,
    body: &Value,
) -> Result<reqwest::Response> {
    let mut token = profile.api_key.clone();

    // No total deadline for a loopback engine — see `total_request_timeout`.
    // The client's `read_timeout` is what catches a stall in that case.
    let deadline = total_request_timeout(profile);

    for attempt in 0..2 {
        let mut builder = client.post(url).json(body);
        if let Some(d) = deadline {
            builder = builder.timeout(d);
        }
        // A model served by a Space App is reached through a *daemon* route
        // (`/api/space/apps/<id>/proxy/v1`), whose gate stops exempting
        // loopback under auth mode `always`. The profile's own credential is
        // empty there by design, so the daemon token has to come from here.
        if let Some((name, value)) = crate::util::internal_auth::header_for(url) {
            builder = builder.header(name, value);
        }
        let request = apply_auth(builder, profile, &token);
        let response = send_once_more_if_dropped(request, profile).await.context("LLM request failed")?;

        let unauthorized = matches!(response.status().as_u16(), 401 | 403);
        let can_retry = attempt == 0 && unauthorized && profile.is_oauth();
        if !can_retry {
            return Ok(response);
        }

        let Some(account_id) = profile.oauth_account_id.as_deref() else {
            return Ok(response);
        };
        let Some(manager) = crate::providers::oauth::global() else {
            return Ok(response);
        };

        debug!(
            "[oauth] {} rejected the token; refreshing",
            profile.provider
        );
        match manager.refresh_account(account_id).await {
            Ok(()) => match manager.access_token(account_id) {
                Some(fresh) => token = fresh,
                None => return Ok(response),
            },
            Err(e) => {
                // Surface the refresh failure rather than the bare 401 — it
                // says whether to re-authorise or just wait.
                bail!("{} token refresh failed: {e}", profile.provider);
            }
        }
    }

    unreachable!("loop returns on the final attempt")
}

/// Send, and once more after a moment when the connection failed or dropped
/// before any answer: a blip on the network, and nothing was generated yet.
/// A timeout is not retried — it already waited out the whole deadline.
async fn send_once_more_if_dropped(
    request: reqwest::RequestBuilder,
    profile: &ModelProfile,
) -> reqwest::Result<reqwest::Response> {
    let Some(again) = request.try_clone() else {
        return request.send().await;
    };
    match request.send().await {
        Err(e) if !e.is_timeout() && (e.is_connect() || e.is_request()) => {
            tracing::warn!(
                "[llm] {} dropped the connection before answering; trying once more",
                profile.provider
            );
            tokio::time::sleep(Duration::from_millis(1500)).await;
            again.send().await
        }
        other => other,
    }
}

// ============================================================================
// Main entry point
// ============================================================================

/// Query an LLM and return the assistant message.
///
/// Routes to OpenAI or Anthropic adapter based on `profile.adapt` or
/// auto-detection from the provider field.
///
/// For `provider = local-candle`, the in-process path always wins so a stale
/// `adapt: "openai"` left over from merged/copied LLM configs cannot force an HTTP request.
pub async fn query_llm(
    client: &Client,
    messages: &[Message],
    system_prompt: &str,
    tools: &[Arc<dyn Tool>],
    cancel: &CancellationToken,
    profile: &ModelProfile,
    thinking: bool,
    stream: bool,
    on_delta: Option<&TextDeltaSink>,
) -> Result<Message> {
    let adapt = effective_adapter(profile);
    info!(
        "[llm] request start provider={} model={} adapter={} stream={} messages={} tools={}",
        profile.provider,
        profile.model_name,
        adapt,
        stream,
        messages.len(),
        tools.len()
    );

    // Persist the full request (system prompt + message history + tool list)
    // to `~/.senclaw/llm_logs/` so prompts can be analyzed/optimized after the
    // fact — e.g. diagnosing an agent stuck re-invoking a skill in a loop.
    let tool_names: Vec<(String, String)> = tools
        .iter()
        .map(|t| (t.name().to_string(), t.description().to_string()))
        .collect();
    crate::util::llm_log::log_request(
        &profile.model_name,
        system_prompt,
        messages,
        &tool_names,
        thinking,
    );

    let result = match adapt {
        "anthropic" => {
            query_anthropic(
                client,
                messages,
                system_prompt,
                tools,
                cancel,
                profile,
                thinking,
                stream,
                on_delta,
            )
            .await
        }
        // Connected via OAuth but not yet speakable: these providers use their
        // own wire formats (OpenAI Responses / Google Code Assist), not
        // chat/completions. Fail loudly — the catch-all below would otherwise
        // POST an OpenAI body at them and surface an unrelated parse error.
        // OpenAI Responses API — Codex and Grok both speak it.
        "codex" => {
            crate::providers::adapters::codex::query_codex(
                client,
                messages,
                system_prompt,
                tools,
                cancel,
                profile,
            )
            .await
        }
        // Google Code Assist — Gemini contents inside a project envelope.
        "antigravity" => {
            crate::providers::adapters::antigravity::query_antigravity(
                client,
                messages,
                system_prompt,
                tools,
                cancel,
                profile,
            )
            .await
        }
        _ => {
            query_openai(
                client,
                messages,
                system_prompt,
                tools,
                cancel,
                profile,
                thinking,
                stream,
                on_delta,
            )
            .await
        }
    };
    match &result {
        Ok(msg) => {
            crate::util::llm_log::log_response(msg);
            let blocks = msg.message.content.len();
            let tool_calls = msg
                .message
                .content
                .iter()
                .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
                .count();
            if blocks == 0 && tool_calls == 0 {
                // Silent upstream failure — adapter parsed 200 OK but no
                // content came through. Log a loud WARN so it's findable in
                // production logs; `conversation.rs` will catch this and
                // surface a SessionError to the UI.
                tracing::warn!(
                    "[llm] EMPTY response provider={} model={} adapter={} \
                     blocks=0 tool_calls=0 — endpoint returned 200 OK with no content. \
                     Check endpoint logs (auth / rate-limit / tool count overload).",
                    profile.provider,
                    profile.model_name,
                    adapt
                );
            } else {
                info!(
                    "[llm] request complete provider={} model={} blocks={} tool_calls={}",
                    profile.provider, profile.model_name, blocks, tool_calls
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                "[llm] request error provider={} model={}: {e}",
                profile.provider,
                profile.model_name
            );
        }
    }
    result
}

/// Adapter names [`query_llm`] dispatches to explicitly. Anything not here hits
/// the catch-all and is sent an OpenAI `chat/completions` body.
pub const ROUTED_ADAPTERS: &[&str] = &[
    "anthropic",
    "codex",
    "antigravity",
    "local-candle-native",
    "local-mlx",
    "openai",
];

/// The subset of [`ROUTED_ADAPTERS`] a Space App may declare for itself.
///
/// Deliberately narrower. `local-mlx` and `local-candle-native` are in-process
/// engines — naming one routes the turn away from HTTP entirely, so the app
/// would be registered and then never called. `codex` and `antigravity` are
/// bespoke wire formats bound to a specific OAuth transport. What is left is the
/// two formats an app can actually serve over its own port.
pub const APP_DECLARABLE_ADAPTERS: &[&str] = &["openai", "anthropic"];

/// Auto-detect adapter from provider name.
fn resolve_adapter(provider: &str) -> &str {
    let lower = provider.to_lowercase();
    if lower.contains("anthropic") || lower.contains("claude") {
        "anthropic"
    } else if is_local_candle_provider(&lower) {
        "local-candle-native"
    } else if is_local_mlx_provider(&lower) {
        "local-mlx"
    } else {
        "openai"
    }
}

fn is_local_candle_provider(lower: &str) -> bool {
    matches!(
        lower,
        "local-candle" | "local-candle-native" | "local-candle-accelerate"
    )
}

fn is_local_mlx_provider(lower: &str) -> bool {
    matches!(lower, "local-mlx" | "local-mlx-native" | "local-mlx-server")
}

/// Claude Opus/Sonnet ≥4.6 (Anthropic provider) accept the `adaptive`
/// thinking type with an `output_config.effort` knob; older models need the
/// legacy `enabled` + `budget_tokens` form. Mirrors sema-core
/// `usesAdaptiveThinking`.
fn uses_adaptive_thinking(profile: &ModelProfile) -> bool {
    if !profile.provider.to_lowercase().contains("anthropic") {
        return false;
    }
    let name = profile.model_name.to_lowercase();
    let Some(pos) = name.find("claude").map(|p| p + "claude".len()) else {
        return false;
    };
    // Parse "claude[-_ ](opus|sonnet)[-_ ]<major>[-._ ]<minor>"
    let rest: &str = &name[pos..];
    let rest = rest.trim_start_matches(['-', '_', ' ']);
    let rest = if let Some(r) = rest.strip_prefix("opus") {
        r
    } else if let Some(r) = rest.strip_prefix("sonnet") {
        r
    } else {
        return false;
    };
    let rest = rest.trim_start_matches(['-', '_', ' ']);
    let mut parts = rest.splitn(3, ['-', '.', '_', ' ']);
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>()) else {
        return false;
    };
    major > 4 || (major == 4 && minor >= 6)
}

/// OpenAI reasoning-model families that take `max_completion_tokens` instead
/// of `max_tokens`. Mirrors sema-core `useMaxCompletionTokens`.
fn uses_max_completion_tokens(model_name: &str) -> bool {
    let lower = model_name.to_lowercase();
    ["o1", "o3", "o4", "gpt-5"]
        .iter()
        .any(|p| lower.starts_with(p))
}

/// Prefer routing implied by `provider`; otherwise use explicit `adapt`.
///
/// Visible to the gateway so brokered one-shot completions decide routing the
/// same way [`query_llm`] does, instead of re-deriving it and disagreeing.
pub(crate) fn effective_adapter(profile: &ModelProfile) -> &str {
    let p = profile.provider.to_lowercase();
    if is_local_candle_provider(&p) {
        return "local-candle-native";
    }
    if is_local_mlx_provider(&p) {
        return "local-mlx";
    }
    profile
        .adapt
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            let lower = s.to_lowercase();
            if is_local_candle_provider(&lower) {
                "local-candle-native"
            } else if is_local_mlx_provider(&lower) {
                "local-mlx"
            } else {
                s
            }
        })
        .unwrap_or_else(|| resolve_adapter(&profile.provider))
}

// ============================================================================


// ============================================================================
// OpenAI adapter
// ============================================================================

pub(crate) fn build_openai_tools(tools: &[Arc<dyn Tool>]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let mut schema = object_parameters(t.input_schema());
            sanitize_schema_node(&mut schema);
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": schema,
                }
            })
        })
        .collect()
}

/// A tool's parameter schema as the object schema every provider requires at
/// the top level. An MCP server may declare `{}`, `{"properties": …}` with no
/// `type`, or nothing at all; OpenAI-compatible servers answer that with 400
/// `invalid_tool_schema` ("tool parameters must be an object schema") and
/// Anthropic with a 400 on `input_schema.type`, failing the whole turn for
/// one tool's sloppy declaration.
pub(crate) fn object_parameters(schema: Value) -> Value {
    match schema {
        Value::Object(mut obj) => {
            match obj.get("type") {
                Some(Value::String(t)) if t == "object" => {}
                // Anything but an object can't be a parameter list: offer no
                // parameters rather than a schema the provider refuses.
                Some(_) => return serde_json::json!({ "type": "object", "properties": {} }),
                None => {
                    obj.insert("type".into(), Value::String("object".into()));
                }
            }
            obj.entry("properties").or_insert_with(|| serde_json::json!({}));
            Value::Object(obj)
        }
        _ => serde_json::json!({ "type": "object", "properties": {} }),
    }
}

/// Replace boolean JSON Schemas (`true`/`false`) with `{"type": "object"}` in
/// every subschema position. schemars emits the boolean form for
/// `serde_json::Value` fields ("any value allowed"), which is valid JSON
/// Schema but rejected by Gemini behind OpenAI-compatible proxies: its proto
/// `Schema` type only accepts objects, so a single `"properties": {"x": true}`
/// 400s the whole request. `additionalProperties` is left untouched — a
/// boolean is the conventional form there and Gemini drops the field.
pub(crate) fn sanitize_schema_node(node: &mut Value) {
    if node.is_boolean() {
        *node = serde_json::json!({"type": "object"});
        return;
    }
    let Some(obj) = node.as_object_mut() else {
        return;
    };
    for key in ["properties", "patternProperties", "$defs", "definitions"] {
        if let Some(map) = obj.get_mut(key).and_then(Value::as_object_mut) {
            for v in map.values_mut() {
                sanitize_schema_node(v);
            }
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(list) = obj.get_mut(key).and_then(Value::as_array_mut) {
            for v in list.iter_mut() {
                sanitize_schema_node(v);
            }
        }
    }
    for key in [
        "items",
        "not",
        "contains",
        "propertyNames",
        "if",
        "then",
        "else",
    ] {
        if let Some(v) = obj.get_mut(key) {
            // Old-draft tuple form: "items": [schema, schema, ...]
            if let Some(list) = v.as_array_mut() {
                for item in list.iter_mut() {
                    sanitize_schema_node(item);
                }
            } else {
                sanitize_schema_node(v);
            }
        }
    }
}

/// Give every historical tool call an id this conversation has not used yet.
/// TurboFieldfare tracks ids across the whole transcript and answers 400
/// `invalid or duplicate historical tool call` when a later turn reuses
/// `ag_call_0`. The matching tool result takes the same new id, in order.
pub(crate) fn uniquify_tool_call_ids(messages: &mut [Value]) {
    use std::collections::{HashMap, HashSet, VecDeque};
    let mut seen = HashSet::new();
    let mut pending: HashMap<String, VecDeque<String>> = HashMap::new();
    for msg in messages.iter_mut() {
        let Some(obj) = msg.as_object_mut() else { continue };
        let role = obj.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "assistant" {
            let Some(calls) = obj.get_mut("tool_calls").and_then(Value::as_array_mut) else {
                continue;
            };
            for call in calls {
                let Some(id) = call.get("id").and_then(Value::as_str).map(str::to_string) else {
                    continue;
                };
                let mut unique = id.clone();
                let mut n = 2u32;
                while !seen.insert(unique.clone()) {
                    unique = format!("{id}__{n}");
                    n += 1;
                }
                call["id"] = serde_json::json!(unique);
                pending.entry(id).or_default().push_back(unique);
            }
        } else if role == "tool" {
            let Some(id) = obj.get("tool_call_id").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            if let Some(mapped) = pending.get_mut(&id).and_then(VecDeque::pop_front) {
                obj.insert("tool_call_id".into(), serde_json::json!(mapped));
            }
        }
    }
}

/// Collapse a union TurboFieldfare cannot render. It accepts a two-branch
/// nullable union and rejects anything else (`only a two-branch nullable
/// union is representable`), which is how FormUI's field `oneOf` fails the
/// whole chat. Object branches become one object whose properties are the
/// union of the branches.
pub(crate) fn fold_gemma_unions(node: &mut Value) {
    if let Some(list) = node.as_array_mut() {
        for item in list {
            fold_gemma_unions(item);
        }
        return;
    }
    let folded = node.as_object().and_then(|obj| {
        let keyword = ["oneOf", "anyOf", "allOf"]
            .into_iter()
            .find(|key| obj.contains_key(*key))?;
        let branches = obj.get(keyword).and_then(Value::as_array).cloned().unwrap_or_default();
        if is_two_branch_nullable(&branches) {
            return None;
        }
        Some(merge_union_branches(&branches, obj))
    });
    if let Some(folded) = folded {
        *node = folded;
    }
    let Some(obj) = node.as_object_mut() else { return };
    let keys: Vec<String> = obj.keys().cloned().collect();
    for key in keys {
        if let Some(child) = obj.get_mut(&key) {
            fold_gemma_unions(child);
        }
    }
}

fn is_two_branch_nullable(branches: &[Value]) -> bool {
    if branches.len() != 2 {
        return false;
    }
    branches
        .iter()
        .filter(|branch| {
            branch.get("type").and_then(Value::as_str) == Some("null")
                && branch.as_object().is_some_and(|obj| obj.len() == 1)
        })
        .count()
        == 1
}

fn merge_union_branches(branches: &[Value], parent: &serde_json::Map<String, Value>) -> Value {
    let mut merged = parent.clone();
    merged.remove("oneOf");
    merged.remove("anyOf");
    merged.remove("allOf");
    let mut props = merged
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for branch in branches {
        let Some(obj) = branch.as_object() else { continue };
        let Some(branch_props) = obj.get("properties").and_then(Value::as_object) else {
            continue;
        };
        for (key, schema) in branch_props {
            match props.get(key) {
                None => {
                    props.insert(key.clone(), schema.clone());
                }
                Some(existing) if existing == schema => {}
                Some(_) => {
                    let ty = schema
                        .get("type")
                        .and_then(Value::as_str)
                        .or_else(|| props.get(key).and_then(|v| v.get("type")).and_then(Value::as_str))
                        .unwrap_or("string");
                    props.insert(key.clone(), serde_json::json!({"type": ty}));
                }
            }
        }
    }
    if !props.is_empty() {
        merged.insert("properties".into(), Value::Object(props));
        merged.entry("type").or_insert(serde_json::json!("object"));
    } else if !merged.contains_key("type") {
        merged.insert("type".into(), serde_json::json!("object"));
    }
    Value::Object(merged)
}

/// TurboFieldfare requires `type` on every schema node. schemars leaves it off
/// for a free-form `serde_json::Value` (the `args` field of `schedule_watch`),
/// and the engine answers 400 `type is required` for the whole chat.
pub(crate) fn ensure_gemma_types(node: &mut Value) {
    if node.is_boolean() {
        *node = serde_json::json!({"type": "object"});
        return;
    }
    if let Some(list) = node.as_array_mut() {
        for item in list {
            ensure_gemma_types(item);
        }
        return;
    }
    let Some(obj) = node.as_object_mut() else { return };
    if let Some(types) = obj.get("type").and_then(Value::as_array).cloned() {
        let names: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
        let concrete: Vec<&str> = names.iter().copied().filter(|name| *name != "null").collect();
        let nullable = names.iter().any(|name| *name == "null");
        let single_nullable = nullable && concrete.len() == 1 && names.len() == 2;
        if !single_nullable {
            let chosen = concrete
                .iter()
                .copied()
                .find(|name| *name == "object")
                .or_else(|| concrete.first().copied())
                .unwrap_or("object");
            obj.insert("type".into(), serde_json::json!(chosen));
            if nullable {
                obj.insert("nullable".into(), serde_json::json!(true));
            }
        }
    }
    if obj.get("type").and_then(Value::as_str).is_none() && !obj.get("type").is_some_and(Value::is_array) {
        let inferred = if obj.contains_key("properties") || obj.contains_key("additionalProperties") {
            "object"
        } else if obj.contains_key("items") {
            "array"
        } else if obj.get("const").is_some_and(Value::is_string)
            || obj.get("enum").and_then(Value::as_array).is_some_and(|values| values.iter().all(Value::is_string))
        {
            "string"
        } else if obj.get("const").is_some_and(Value::is_number) {
            "number"
        } else if obj.get("const").is_some_and(Value::is_boolean) {
            "boolean"
        } else {
            "object"
        };
        obj.insert("type".into(), serde_json::json!(inferred));
    }
    let keys: Vec<String> = obj.keys().cloned().collect();
    for key in keys {
        let Some(child) = obj.get_mut(&key) else { continue };
        match key.as_str() {
            "properties" | "patternProperties" | "$defs" | "definitions" => {
                if let Some(map) = child.as_object_mut() {
                    for schema in map.values_mut() {
                        ensure_gemma_types(schema);
                    }
                }
            }
            "oneOf" | "anyOf" | "allOf" | "prefixItems" => {
                if let Some(list) = child.as_array_mut() {
                    for schema in list {
                        ensure_gemma_types(schema);
                    }
                }
            }
            "items" | "additionalProperties" | "not" | "if" | "then" | "else" | "contains" | "propertyNames" => {
                ensure_gemma_types(child);
            }
            _ => {}
        }
    }
}

/// Convert internal [`Message`] history to OpenAI Chat Completions `messages` JSON.
///
/// OpenAI-compatible APIs (DeepSeek, OpenRouter, etc.) expect:
/// - `assistant` + tools: `tool_calls` on the assistant message, **not** `content` parts with `tool_use`.
/// - tool outputs: separate messages with `role: "tool"` and `tool_call_id`.
///
/// Our internal format mirrors Anthropic (`ToolUse` / `ToolResult` inside `content`), so we expand
/// that here — otherwise providers reject the body (`unknown variant tool_use`).
///
/// Thinking / reasoning: [`ContentBlock::Thinking`] is serialized as `reasoning_content` on
/// `assistant` messages (required by DeepSeek and similar when thinking mode is on).
pub(crate) fn openai_messages_for_api(
    messages: &[Message],
    system_prompt: &str,
) -> Result<Vec<Value>> {
    let mut api_msgs: Vec<Value> = Vec::new();

    if !system_prompt.is_empty() {
        api_msgs.push(serde_json::json!({
            "role": "system",
            "content": system_prompt,
        }));
    }

    for msg in messages {
        match msg.message.role.as_str() {
            "user" => {
                let mut text_acc = String::new();
                let mut content_parts: Vec<Value> = Vec::new();
                for b in &msg.message.content {
                    match b {
                        ContentBlock::Text { text } => {
                            if !text_acc.is_empty() {
                                text_acc.push('\n');
                            }
                            text_acc.push_str(text);
                        }
                        ContentBlock::Image { source } => {
                            // Flush text accumulator first if not empty
                            if !text_acc.is_empty() {
                                content_parts.push(serde_json::json!({
                                    "type": "text",
                                    "text": text_acc,
                                }));
                                text_acc.clear();
                            }
                            // Add image content
                            content_parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", source.media_type, source.data),
                                }
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            // Flush any accumulated content first
                            if !text_acc.is_empty() || !content_parts.is_empty() {
                                if !text_acc.is_empty() {
                                    content_parts.push(serde_json::json!({
                                        "type": "text",
                                        "text": text_acc,
                                    }));
                                    text_acc.clear();
                                }
                                api_msgs.push(serde_json::json!({
                                    "role": "user",
                                    "content": content_parts,
                                }));
                                content_parts.clear();
                            }
                            api_msgs.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                        }
                        ContentBlock::ControlSignal { .. } => {}
                        ContentBlock::Thinking { .. } => {}
                        ContentBlock::ToolUse { .. } => {
                            bail!("OpenAI adapter: unexpected ToolUse block in user message");
                        }
                    }
                }
                // Flush any remaining content
                if !text_acc.is_empty() || !content_parts.is_empty() {
                    if !text_acc.is_empty() {
                        content_parts.push(serde_json::json!({
                            "type": "text",
                            "text": text_acc,
                        }));
                    }
                    // Single text part → plain-string content (what OpenAI-shape
                    // providers expect for text-only turns). The parts are
                    // `{type:"text",text:...}` OBJECTS, so match on the field —
                    // matching `Value::String` here never fires.
                    let single_text = content_parts.len() == 1
                        && content_parts[0].get("type").and_then(|t| t.as_str()) == Some("text");
                    if single_text {
                        api_msgs.push(serde_json::json!({
                            "role": "user",
                            "content": content_parts[0]["text"],
                        }));
                    } else {
                        api_msgs.push(serde_json::json!({
                            "role": "user",
                            "content": content_parts,
                        }));
                    }
                }
            }
            "assistant" => {
                let mut text_buf = String::new();
                let mut reasoning_buf = String::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for b in &msg.message.content {
                    match b {
                        ContentBlock::Text { text } => text_buf.push_str(text),
                        ContentBlock::Thinking { thinking } => {
                            // DeepSeek (and some OpenAI-compatible "thinking" models) require
                            // `reasoning_content` to be echoed on the next request.
                            reasoning_buf.push_str(thinking);
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            let args =
                                serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                            tool_calls.push(serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": args,
                                }
                            }));
                        }
                        ContentBlock::ToolResult { .. } => {}
                        ContentBlock::ControlSignal { .. } => {}
                        ContentBlock::Image { .. } => {
                            // Images in assistant messages are not standard, but handle gracefully
                            tracing::warn!(
                                "OpenAI adapter: unexpected Image block in assistant message"
                            );
                        }
                    }
                }

                let mut obj = serde_json::Map::new();
                obj.insert("role".into(), serde_json::json!("assistant"));
                if text_buf.is_empty() {
                    obj.insert("content".into(), serde_json::Value::Null);
                } else {
                    obj.insert("content".into(), serde_json::json!(text_buf));
                }
                if !reasoning_buf.is_empty() {
                    obj.insert("reasoning_content".into(), serde_json::json!(reasoning_buf));
                }
                if !tool_calls.is_empty() {
                    obj.insert("tool_calls".into(), serde_json::json!(tool_calls));
                }
                api_msgs.push(Value::Object(obj));
            }
            other => {
                bail!("OpenAI adapter: unsupported message role {other}");
            }
        }
    }

    Ok(api_msgs)
}

// ============================================================================


async fn query_openai(
    client: &Client,
    messages: &[Message],
    system_prompt: &str,
    tools: &[Arc<dyn Tool>],
    cancel: &CancellationToken,
    profile: &ModelProfile,
    _thinking: bool,
    stream: bool,
    on_delta: Option<&TextDeltaSink>,
) -> Result<Message> {
    let url = format!(
        "{}/chat/completions",
        profile.base_url.trim_end_matches('/')
    );

    let mut api_messages = openai_messages_for_api(messages, system_prompt)?;
    let mut openai_tools = if tools.is_empty() {
        None
    } else {
        Some(build_openai_tools(tools))
    };
    // TurboFieldfare rejects a chat whose history reuses a tool-call id, and
    // rejects a tool schema whose `oneOf` is not a two-branch nullable union
    // (FormUI's field list). Rewrite only this engine's request.
    if profile.model_name.starts_with("gturbo-") {
        uniquify_tool_call_ids(&mut api_messages);
        if let Some(tools) = openai_tools.as_mut() {
            for tool in tools.iter_mut() {
                if let Some(params) = tool
                    .get_mut("function")
                    .and_then(|f| f.get_mut("parameters"))
                {
                    fold_gemma_unions(params);
                    ensure_gemma_types(params);
                }
            }
        }
    }

    // OpenAI reasoning models (o1/o3/o4/gpt-5 families) reject `max_tokens`
    // and require `max_completion_tokens` instead.
    let max_tokens_key = if uses_max_completion_tokens(&profile.model_name) {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    let mut body = serde_json::json!({
        "model": profile.model_name,
        "messages": api_messages,
        "stream": stream,
    });
    body[max_tokens_key] = serde_json::json!(profile.max_tokens);

    // Ask the server to emit a final usage chunk so we can capture real token
    // counts from streamed responses (no-op for providers that ignore it).
    if stream {
        body["stream_options"] = serde_json::json!({ "include_usage": true });
    }

    if let Some(ref t) = openai_tools {
        body["tools"] = serde_json::Value::Array(t.clone());
    }

    debug!("[openai] POST {url}");

    // Check for cancellation before sending
    if cancel.is_cancelled() {
        bail!("Request cancelled before send");
    }

    let response = post_authed(client, &url, profile, &body)
        .await
        .context("OpenAI request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("OpenAI API error ({status}): {body}");
    }

    // A server that answers a stream request with plain JSON is reporting an
    // error under a 200 (LM Studio does this for an unknown path:
    // `{"error":"Unexpected endpoint or method. (POST /chat/completions)"}`)
    // or ignoring `stream`. Read as SSE, either comes back as an empty answer:
    // the turn is retried and finally reported as EMPTY_COMPLETION with a
    // hint about tool counts, while the server's own message is never shown.
    let is_event_stream = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"));
    if stream && is_event_stream {
        return parse_openai_stream(response, cancel, on_delta).await;
    }
    let json: Value = response.json().await.context("OpenAI JSON parse")?;
    if let Some(err) = openai_body_error(&json) {
        bail!("OpenAI API error (200 OK): {err}{}", base_url_hint(&profile.base_url));
    }
    parse_openai_non_stream(&json)
}

/// The error a 200 response body carries, in either shape servers use:
/// `{"error": "text"}` (LM Studio) or `{"error": {"message": …}}` (OpenAI).
fn openai_body_error(json: &Value) -> Option<String> {
    match json.get("error")? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| other.to_string())),
    }
}

/// A hint for the commonest misconfiguration behind an "unknown endpoint":
/// a base URL with no path, when the server serves its OpenAI API under `/v1`
/// (LM Studio, Ollama, vLLM, llama.cpp all do).
fn base_url_hint(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let path = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest).split_once('/').map(|(_, p)| p);
    match path {
        None | Some("") => format!(" — the base URL has no path; OpenAI-compatible servers usually serve under /v1, try {trimmed}/v1"),
        Some(_) => String::new(),
    }
}

async fn parse_openai_stream(
    response: reqwest::Response,
    cancel: &CancellationToken,
    on_delta: Option<&TextDeltaSink>,
) -> Result<Message> {
    let mut stream = response.bytes_stream();
    let mut text_buf = String::new();
    let mut reasoning_buf = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut model_name = String::new();
    let mut usage: Option<RawUsage> = None;
    let mut lines = SseLines::default();

    let mut ended = false;
    while !ended {
        if cancel.is_cancelled() {
            bail!("Stream cancelled");
        }

        // On stream end, flush the buffer once: a provider that omits the
        // trailing newline would otherwise strand its last event.
        // Wait for the next chunk *or* the cancel. A local model's prefill
        // sends nothing for tens of seconds, and a cancel noticed only when
        // the next byte arrived (a keep-alive, or the first token) kept the
        // runtime computing an answer nobody wanted.
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => bail!("Stream cancelled"),
            next = stream.next() => next,
        };
        let batch = match next {
            Some(chunk_result) => {
                let chunk = chunk_result.context("OpenAI stream chunk error")?;
                lines.push(&chunk)
            }
            None => {
                ended = true;
                lines.flush().into_iter().collect()
            }
        };

        for line in batch {
            let line = line.as_str();
            if line.is_empty() || line == "data: [DONE]" {
                continue;
            }
            if !line.starts_with("data: ") {
                continue;
            }
            let json_str = &line[6..];
            let delta: Value = match serde_json::from_str(json_str) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("[llm-stream] openai: undecodable SSE line ({e}) — dropped");
                    continue;
                }
            };

            if model_name.is_empty() {
                model_name = delta["model"].as_str().unwrap_or("").to_string();
            }

            // With `stream_options.include_usage`, the final chunk carries a
            // top-level `usage` object (and an empty `choices` array).
            if let Some(u) = RawUsage::from_json(&delta["usage"]) {
                usage = Some(u);
            }

            if let Some(choices) = delta["choices"].as_array() {
                for choice in choices {
                    if let Some(delta) = choice.get("delta") {
                        if let Some(content) = delta["content"].as_str() {
                            text_buf.push_str(content);
                            if !content.is_empty() {
                                if let Some(sink) = on_delta {
                                    sink(content);
                                }
                            }
                        }
                        if let Some(reasoning) = delta["reasoning_content"].as_str() {
                            reasoning_buf.push_str(reasoning);
                        }
                        if let Some(tc_deltas) = delta["tool_calls"].as_array() {
                            for tc in tc_deltas {
                                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                                while tool_calls.len() <= idx {
                                    tool_calls.push(serde_json::json!({
                                        "id": "",
                                        "function": {"name": "", "arguments": ""}
                                    }));
                                }
                                if let Some(id) = tc["id"].as_str() {
                                    tool_calls[idx]["id"] = Value::String(id.to_string());
                                }
                                if let Some(func) = tc.get("function") {
                                    if let Some(name) = func["name"].as_str() {
                                        tool_calls[idx]["function"]["name"] =
                                            Value::String(format!(
                                                "{}{}",
                                                tool_calls[idx]["function"]["name"]
                                                    .as_str()
                                                    .unwrap_or(""),
                                                name
                                            ));
                                    }
                                    if let Some(args) = func["arguments"].as_str() {
                                        tool_calls[idx]["function"]["arguments"] =
                                            Value::String(format!(
                                                "{}{}",
                                                tool_calls[idx]["function"]["arguments"]
                                                    .as_str()
                                                    .unwrap_or(""),
                                                args
                                            ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    build_assistant_message(&text_buf, &reasoning_buf, &tool_calls, usage)
}

fn parse_openai_non_stream(json: &Value) -> Result<Message> {
    let choice = &json["choices"][0];
    let msg = &choice["message"];

    let text = msg["content"].as_str().unwrap_or("").to_string();
    let reasoning = msg
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut tool_calls = Vec::new();
    if let Some(tc_list) = msg["tool_calls"].as_array() {
        for tc in tc_list {
            tool_calls.push(tc.clone());
        }
    }

    let usage = RawUsage::from_json(&json["usage"]);
    build_assistant_message(&text, &reasoning, &tool_calls, usage)
}

// ============================================================================
// Anthropic adapter
// ============================================================================

fn anthropic_tools_for_api(tools: &[Arc<dyn Tool>]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let schema = object_parameters(t.input_schema());
            serde_json::json!({
                "name": t.name(),
                "description": t.description(),
                "input_schema": schema,
            })
        })
        .collect()
}

fn anthropic_messages_for_api(messages: &[Message]) -> Vec<Value> {
    let mut api_msgs: Vec<Value> = Vec::new();
    for msg in messages {
        let role = &msg.message.role;
        let content = anthropic_content_blocks(&msg.message.content);
        api_msgs.push(serde_json::json!({
            "role": role,
            "content": content,
        }));
    }
    api_msgs
}

fn anthropic_content_blocks(blocks: &[ContentBlock]) -> Value {
    let mut parts: Vec<Value> = Vec::new();
    for b in blocks {
        match b {
            ContentBlock::Text { text } => {
                parts.push(serde_json::json!({"type": "text", "text": text}));
            }
            ContentBlock::ToolUse { id, name, input } => {
                parts.push(serde_json::json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                }));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                parts.push(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                    "is_error": is_error,
                }));
            }
            ContentBlock::Thinking { thinking } => {
                parts.push(serde_json::json!({
                    "type": "thinking",
                    "thinking": thinking,
                }));
            }
            ContentBlock::Image { source } => {
                parts.push(serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": source.source_type,
                        "media_type": source.media_type,
                        "data": source.data,
                    }
                }));
            }
            ContentBlock::ControlSignal { .. } => {}
        }
    }
    Value::Array(parts)
}

async fn query_anthropic(
    client: &Client,
    messages: &[Message],
    system_prompt: &str,
    tools: &[Arc<dyn Tool>],
    cancel: &CancellationToken,
    profile: &ModelProfile,
    thinking: bool,
    stream: bool,
    on_delta: Option<&TextDeltaSink>,
) -> Result<Message> {
    let url = format!("{}/v1/messages", profile.base_url.trim_end_matches('/'));

    let api_messages = anthropic_messages_for_api(messages);
    let anthropic_tools = if tools.is_empty() {
        None
    } else {
        Some(anthropic_tools_for_api(tools))
    };

    let mut body = serde_json::json!({
        "model": profile.model_name,
        "max_tokens": profile.max_tokens,
        "messages": api_messages,
        "stream": stream,
    });

    if !system_prompt.is_empty() {
        body["system"] = Value::String(system_prompt.to_string());
    }

    if let Some(ref t) = anthropic_tools {
        body["tools"] = serde_json::Value::Array(t.clone());
    }

    // Thinking parameter by model capability: Claude Opus/Sonnet ≥4.6 take
    // adaptive thinking with an effort knob; older models take the legacy
    // enabled/budget form (budget capped below max_tokens).
    if uses_adaptive_thinking(profile) {
        if thinking {
            body["thinking"] = serde_json::json!({ "type": "adaptive" });
            body["output_config"] = serde_json::json!({ "effort": "medium" });
        }
    } else if thinking {
        let budget = (profile.max_tokens / 2).clamp(1024, 4096);
        body["thinking"] = serde_json::json!({
            "type": "enabled",
            "budget_tokens": budget,
        });
    }

    debug!("[anthropic] POST {url}");

    if cancel.is_cancelled() {
        bail!("Request cancelled before send");
    }

    let response = post_authed(client, &url, profile, &body)
        .await
        .context("Anthropic request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("Anthropic API error ({status}): {body}");
    }

    if stream {
        parse_anthropic_stream(response, cancel, on_delta).await
    } else {
        let json: Value = response.json().await.context("Anthropic JSON parse")?;
        parse_anthropic_non_stream(&json)
    }
}

async fn parse_anthropic_stream(
    response: reqwest::Response,
    cancel: &CancellationToken,
    on_delta: Option<&TextDeltaSink>,
) -> Result<Message> {
    let mut stream = response.bytes_stream();
    let mut text_buf = String::new();
    let mut reasoning_buf = String::new();
    let mut tool_use_blocks: Vec<Value> = Vec::new();
    let mut current_tool_idx: Option<usize> = None;
    let mut usage: Option<RawUsage> = None;
    let mut lines = SseLines::default();

    let mut ended = false;
    while !ended {
        if cancel.is_cancelled() {
            bail!("Stream cancelled");
        }

        // Wait for the next chunk *or* the cancel. A local model's prefill
        // sends nothing for tens of seconds, and a cancel noticed only when
        // the next byte arrived (a keep-alive, or the first token) kept the
        // runtime computing an answer nobody wanted.
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => bail!("Stream cancelled"),
            next = stream.next() => next,
        };
        let batch = match next {
            Some(chunk_result) => {
                let chunk = chunk_result.context("Anthropic stream chunk error")?;
                lines.push(&chunk)
            }
            None => {
                ended = true;
                lines.flush().into_iter().collect()
            }
        };

        for line in batch {
            let line = line.as_str();
            if line.is_empty() {
                continue;
            }
            if !line.starts_with("data: ") {
                continue;
            }
            let json_str = &line[6..];
            let event: Value = match serde_json::from_str(json_str) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("[llm-stream] anthropic: undecodable SSE line ({e}) — dropped");
                    continue;
                }
            };

            let event_type = event["type"].as_str().unwrap_or("");

            match event_type {
                "message_start" => {
                    // Carries input/cache token counts (output is ~1 here).
                    if let Some(u) = RawUsage::from_json(&event["message"]["usage"]) {
                        usage.get_or_insert_with(RawUsage::default).merge(&u);
                    }
                }
                "content_block_start" => {
                    if let Some(cb) = event.get("content_block") {
                        match cb["type"].as_str().unwrap_or("") {
                            "tool_use" => {
                                let idx = cb["index"].as_u64().unwrap_or(0) as usize;
                                current_tool_idx = Some(idx);
                                while tool_use_blocks.len() <= idx {
                                    tool_use_blocks.push(serde_json::json!({
                                        "id": "",
                                        "name": "",
                                        "input": {},
                                    }));
                                }
                                tool_use_blocks[idx]["id"] = cb["id"].clone();
                                tool_use_blocks[idx]["name"] = cb["name"].clone();
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = event.get("delta") {
                        match delta["type"].as_str().unwrap_or("") {
                            "text_delta" => {
                                if let Some(t) = delta["text"].as_str().filter(|t| !t.is_empty()) {
                                    text_buf.push_str(t);
                                    if let Some(sink) = on_delta {
                                        sink(t);
                                    }
                                }
                            }
                            "thinking_delta" => {
                                if let Some(t) = delta["thinking"].as_str() {
                                    reasoning_buf.push_str(t);
                                }
                            }
                            "input_json_delta" => {
                                if let Some(json_str) = delta["partial_json"].as_str() {
                                    if let Some(idx) = current_tool_idx {
                                        if idx < tool_use_blocks.len() {
                                            let current = tool_use_blocks[idx]["input"]
                                                .as_str()
                                                .unwrap_or("");
                                            let merged = format!("{current}{json_str}");
                                            // Store as string during accumulation, parse at end
                                            tool_use_blocks[idx]["_input_json"] =
                                                Value::String(merged);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "message_delta" => {
                    // Carries the cumulative output_tokens (and stop_reason).
                    if let Some(u) = RawUsage::from_json(&event["usage"]) {
                        usage.get_or_insert_with(RawUsage::default).merge(&u);
                    }
                }
                _ => {}
            }
        }
    }

    // Convert accumulated JSON strings to parsed objects
    for block in &mut tool_use_blocks {
        if let Some(json_str) = block.get("_input_json").and_then(|v| v.as_str()) {
            block["input"] =
                serde_json::from_str(json_str).unwrap_or(Value::Object(Default::default()));
        }
    }

    build_assistant_message_anthropic(&text_buf, &reasoning_buf, &tool_use_blocks, usage)
}

fn parse_anthropic_non_stream(json: &Value) -> Result<Message> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_use_blocks: Vec<Value> = Vec::new();

    if let Some(content) = json["content"].as_array() {
        for block in content {
            match block["type"].as_str().unwrap_or("") {
                "text" => {
                    if let Some(t) = block["text"].as_str() {
                        text.push_str(t);
                    }
                }
                "thinking" => {
                    if let Some(t) = block["thinking"].as_str() {
                        reasoning.push_str(t);
                    }
                }
                "tool_use" => {
                    tool_use_blocks.push(block.clone());
                }
                _ => {}
            }
        }
    }

    let usage = RawUsage::from_json(&json["usage"]);
    build_assistant_message_anthropic(&text, &reasoning, &tool_use_blocks, usage)
}

// ============================================================================
// Message construction helpers
// ============================================================================

pub(crate) fn build_assistant_message(
    text: &str,
    reasoning: &str,
    tool_calls: &[Value],
    usage: Option<RawUsage>,
) -> Result<Message> {
    let mut content: Vec<ContentBlock> = Vec::new();

    if !reasoning.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking: reasoning.to_string(),
        });
    }

    if !text.is_empty() {
        content.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }

    for tc in tool_calls {
        let id = tc["id"].as_str().unwrap_or("").to_string();
        let name = if let Some(n) = tc["function"]["name"].as_str() {
            n.to_string()
        } else {
            tc["name"].as_str().unwrap_or("").to_string()
        };
        let input = if let Some(args) = tc["function"]["arguments"].as_str() {
            serde_json::from_str(args).unwrap_or(Value::Object(Default::default()))
        } else {
            tc["input"].clone()
        };

        content.push(ContentBlock::ToolUse { id, name, input });
    }

    Ok(Message {
        msg_type: "assistant".to_string(),
        message: MessagePayload {
            role: "assistant".to_string(),
            content,
        },
        uuid: uuid::Uuid::new_v4().to_string(),
        usage,
    })
}

fn build_assistant_message_anthropic(
    text: &str,
    reasoning: &str,
    tool_uses: &[Value],
    usage: Option<RawUsage>,
) -> Result<Message> {
    let mut content: Vec<ContentBlock> = Vec::new();

    if !reasoning.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking: reasoning.to_string(),
        });
    }

    if !text.is_empty() {
        content.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }

    for tu in tool_uses {
        let id = tu["id"].as_str().unwrap_or("").to_string();
        let name = tu["name"].as_str().unwrap_or("").to_string();
        let input = tu["input"].clone();
        content.push(ContentBlock::ToolUse { id, name, input });
    }

    Ok(Message {
        msg_type: "assistant".to_string(),
        message: MessagePayload {
            role: "assistant".to_string(),
            content,
        },
        uuid: uuid::Uuid::new_v4().to_string(),
        usage,
    })
}

// ============================================================================
// Error classification (mirrors TS emitSessionError)
// ============================================================================

/// Classified error from an LLM call.
#[derive(Debug, Clone)]
pub struct LlmError {
    pub code: String,
    pub message: String,
    pub error_type: String,
    pub is_context_length: bool,
}

impl LlmError {
    pub fn classify(err: &anyhow::Error) -> Self {
        let msg = err.to_string();
        let msg_lower = msg.to_lowercase();
        // The outermost message names the step ("Antigravity request
        // failed"); a transport failure's reason is further down the chain.
        // Without it a dropped connection read as unknown and reset the
        // whole session. The chain can carry the request URL — some
        // providers put the API key there — so only the root cause, with
        // any URL masked, is ever shown.
        let chain_lower = format!("{err:#}").to_lowercase();
        let cause = mask_urls(&err.root_cause().to_string());

        // Check for cancellation first — not an error to report
        if msg_lower.contains("cancelled") || msg_lower.contains("aborted") {
            return Self {
                code: "CANCELLED".into(),
                message: msg.clone(),
                error_type: "cancelled".into(),
                is_context_length: false,
            };
        }

        // OpenAI context length error
        if msg_lower.contains("context_length_exceeded")
            || msg_lower.contains("maximum context length")
            || msg_lower.contains("reduce the length")
        {
            return Self {
                code: "CONTEXT_TOO_LONG".into(),
                message: "Context length exceeded".into(),
                error_type: "context_length_exceeded".into(),
                is_context_length: true,
            };
        }

        // HTTP status codes
        if let Some(code) = extract_http_status(&msg) {
            let error_code = format!("API_ERROR_{code}");
            return Self {
                code: error_code,
                message: msg.clone(),
                error_type: "api_error".into(),
                is_context_length: false,
            };
        }

        // Auth — **only** clear HTTP / API-key signals. Broad `contains("auth")` or bare
        // `401` false-positive on local MLX + tools (paths like `.../authors/...`, "oauth"
        // in JSON schema, tensor sizes mentioning 401, etc.) and surfaces misleading
        // "check API key" even though no remote API is involved.
        if looks_like_http_auth_failure(&msg_lower) {
            return Self {
                code: "AUTH_ERROR".into(),
                message: "API authentication failed — check API key".into(),
                error_type: "api_error".into(),
                is_context_length: false,
            };
        }

        // Rate limit — avoid bare `429` (can appear in unrelated numeric errors).
        if msg_lower.contains("rate limit")
            || msg_lower.contains("too many requests")
            || msg_lower.contains("429 too many")
            || msg_lower.contains("http 429")
            || msg_lower.contains("status 429")
        {
            return Self {
                code: "RATE_LIMIT".into(),
                message: "API rate limit exceeded — retry later".into(),
                error_type: "api_error".into(),
                is_context_length: false,
            };
        }

        // Network — bare `timeout` / `connection` / `fetch` match MCP tool JSON (timeout_ms,
        // "connection state", "Fetch …") when errors embed the full `tools` payload; classify
        // only clear transport / HTTP-client signals.
        if looks_like_network_transport_failure(&msg_lower) || looks_like_network_transport_failure(&chain_lower) {
            return Self {
                code: "NETWORK_ERROR".into(),
                message: format!("Network error — check connectivity ({cause})"),
                error_type: "api_error".into(),
                is_context_length: false,
            };
        }

        // JSON / body parse — the word `json` appears in every `$schema` URL inside tool defs;
        // avoid treating template / MLX errors as "API response parse" unless it looks like serde/JSON.
        if looks_like_response_parse_failure(&msg_lower) {
            return Self {
                code: "API_RESPONSE_ERROR".into(),
                message: format!("API response parse error: {msg}"),
                error_type: "api_error".into(),
                is_context_length: false,
            };
        }

        // Default
        Self {
            code: "UNKNOWN_ERROR".into(),
            message: if cause == msg { msg.clone() } else { format!("{msg}: {cause}") },
            error_type: "api_error".into(),
            is_context_length: false,
        }
    }

    /// Whether this error should be surfaced as `session:error`.
    pub fn should_emit(&self) -> bool {
        self.error_type != "cancelled"
    }

    /// Convert to SessionErrorData for emission.
    pub fn to_session_error(&self) -> SessionErrorData {
        SessionErrorData {
            error_type: self.error_type.clone(),
            error: SessionErrorDetail {
                code: self.code.clone(),
                message: self.message.clone(),
                details: None,
            },
        }
    }
}

/// True when `msg_lower` reads like an HTTP/API credential failure (not substring "auth"
/// inside unrelated words such as `authors`, `oauth`, or bare `401` in tensor sizes).
fn looks_like_http_auth_failure(msg_lower: &str) -> bool {
    const PHRASES: &[&str] = &[
        "401 unauthorized",
        "http 401",
        "https 401",
        "status 401",
        "status: 401",
        "status = 401",
        "unauthorized",
        "invalid_api_key",
        "invalid api key",
        "incorrect api key",
        "missing api key",
        "api key missing",
        "api key not found",
        "api key expired",
        "authentication failed",
        "access token invalid",
        "access token expired",
        "no api key",
        "wrong api key",
        "bearer token",
    ];
    PHRASES.iter().any(|p| msg_lower.contains(p))
}

/// True for HTTP client / OS transport failures — not substrings like `timeout_ms` inside MCP schemas.
fn looks_like_network_transport_failure(msg_lower: &str) -> bool {
    const PHRASES: &[&str] = &[
        "operation timed out",
        "request timed out",
        "timed out waiting",
        "deadline has elapsed",
        "connection refused",
        "connection reset",
        "connection aborted",
        "broken pipe",
        "unexpected eof",
        "error sending request",
        "error trying to connect",
        "could not connect",
        "failed to connect",
        "tcp connect",
        "dns error",
        "failed to lookup",
        "name or service not known",
        "getaddrinfo",
        "ssl error",
        "tls handshake",
        "certificate verify",
        "reqwest::",
        "hyper::",
        "http connect",
        "network unreachable",
        "host unreachable",
        "no route to host",
    ];
    PHRASES.iter().any(|p| msg_lower.contains(p))
}

/// True when the failure reads like JSON/body parsing — not `$schema` URLs in embedded tool JSON.
fn looks_like_response_parse_failure(msg_lower: &str) -> bool {
    const PHRASES: &[&str] = &[
        "serde_json::error",
        "serde_json::err",
        "invalid escape",
        "trailing characters",
        "expected value at line",
        "key must be a string",
        "invalid json",
        "failed to parse json",
        "error decoding response body",
        "error decoding response",
        "json parse error",
        "unexpected end of json",
        "expected `,` or `}`",
        "expected `:`",
    ];
    PHRASES.iter().any(|p| msg_lower.contains(p))
}

/// `s` with every URL replaced by `<url>`.
fn mask_urls(s: &str) -> String {
    s.split(' ')
        .map(|w| if w.contains("://") { "<url>" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod transport_error_tests {
    use super::*;

    fn wrapped(root: anyhow::Error) -> anyhow::Error {
        root.context("error sending request for url (https://llm.test/v1?key=SECRET)")
            .context("LLM request failed")
            .context("Antigravity request failed")
    }

    #[test]
    fn a_dropped_connection_keeps_the_session() {
        let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "Connection reset by peer (os error 54)");
        let e = LlmError::classify(&wrapped(anyhow::Error::new(reset)));
        assert_eq!(e.code, "NETWORK_ERROR", "{}", e.message);
        assert!(e.message.contains("Connection reset by peer"), "{}", e.message);
        assert!(!e.message.contains("SECRET"), "{}", e.message);
    }

    #[test]
    fn an_unknown_failure_says_why_without_the_url() {
        let e = LlmError::classify(&anyhow::anyhow!("see https://llm.test/?key=SECRET for details").context("Antigravity request failed"));
        assert_eq!(e.code, "UNKNOWN_ERROR");
        assert_eq!(e.message, "Antigravity request failed: see <url> for details");
    }

    /// The first connection is dropped before any answer; the second answers.
    #[tokio::test]
    async fn a_connection_dropped_before_answering_is_tried_once_more() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            drop(first);
            let (mut second, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = second.read(&mut buf).await;
            let _ = second.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok").await;
        });
        let client = Client::new();
        let request = client.post(format!("http://{addr}/v1")).json(&serde_json::json!({ "x": 1 }));
        let profile = ModelProfile::default();
        let response = send_once_more_if_dropped(request, &profile).await.expect("answered on the second try");
        assert_eq!(response.text().await.unwrap(), "ok");
    }
}

fn extract_http_status(msg: &str) -> Option<u16> {
    // Match patterns like "API error (429)" or "status: 500"
    if let Some(start) = msg.find('(') {
        let rest = &msg[start + 1..];
        if let Some(end) = rest.find(')') {
            if let Ok(code) = rest[..end].parse::<u16>() {
                return Some(code);
            }
        }
    }
    None
}

// ============================================================================
// Re-export helper for creating a configured reqwest client
// ============================================================================

pub fn create_llm_client() -> Result<Client> {
    // See `ZenEngine::new` — read timeout, not total. The per-request deadline
    // that still applies to remote providers is set in `post_authed`.
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(STREAM_STALL_TIMEOUT)
        .build()
        .context("Failed to create HTTP client")
}

#[cfg(test)]
mod sse_tests {
    use super::SseLines;

    #[test]
    fn splits_complete_lines_and_keeps_the_remainder() {
        let mut lines = SseLines::default();
        assert_eq!(lines.push(b"data: {\"a\":1}\n"), vec!["data: {\"a\":1}"]);
        // No newline yet — nothing may be emitted.
        assert!(lines.push(b"data: {\"b\"").is_empty());
        assert_eq!(lines.push(b":2}\n"), vec!["data: {\"b\":2}"]);
    }

    /// The bug this type exists for: a `data:` line cut by the network used to
    /// be decoded on its own, fail to parse, and be dropped — losing the text
    /// it carried from both the stream and the final message.
    #[test]
    fn reassembles_a_line_split_across_three_chunks() {
        let mut lines = SseLines::default();
        let mut out = Vec::new();
        for part in [
            b"data: {\"tex".as_ref(),
            b"t\":\"hel".as_ref(),
            b"lo\"}\n\n".as_ref(),
        ] {
            out.extend(lines.push(part));
        }
        assert_eq!(out, vec!["data: {\"text\":\"hello\"}", ""]);
    }

    /// A chunk boundary inside a multi-byte character must not corrupt it —
    /// decoding per chunk turned Vietnamese diacritics into replacement chars.
    #[test]
    fn survives_a_split_inside_a_utf8_sequence() {
        let text = "chào bạn";
        let payload = format!("data: {text}\n");
        let bytes = payload.as_bytes();
        let cut = payload.find('à').unwrap() + 1; // mid-sequence

        let mut lines = SseLines::default();
        assert!(lines.push(&bytes[..cut]).is_empty());
        assert_eq!(lines.push(&bytes[cut..]), vec![format!("data: {text}")]);
    }

    #[test]
    fn flush_returns_a_trailing_line_without_a_newline() {
        let mut lines = SseLines::default();
        assert!(lines.push(b"data: [DONE]").is_empty());
        assert_eq!(lines.flush().as_deref(), Some("data: [DONE]"));
        assert_eq!(lines.flush(), None);
    }
}

#[cfg(test)]
mod loopback_tests {
    use super::{is_loopback_endpoint, total_request_timeout, REQUEST_TIMEOUT};

    #[test]
    fn recognises_every_shape_a_local_engine_is_configured_with() {
        for url in [
            "http://127.0.0.1:18788/api/space/apps/mlx-llm/proxy/v1",
            "http://127.0.0.1:11434/v1",  // Ollama
            "http://localhost:1234/v1",   // LM Studio
            "http://LOCALHOST:8080/v1",   // case is not significant in a host
            "http://[::1]:8080/v1",       // IPv6 literal
            "http://127.1.2.3:8080/v1",   // the whole 127.0.0.0/8 block
            "http://app.localhost:3000",  // RFC 6761
            "http://user:pass@127.0.0.1:8080/v1",
            "http://127.0.0.1:8080",      // no path at all
        ] {
            assert!(is_loopback_endpoint(url), "{url} should be loopback");
        }
    }

    /// The direction this must not get wrong. A prefix or `contains` test says
    /// yes to every one of these, and each would silently drop the deadline on
    /// a remote endpoint an attacker chose the hostname of.
    #[test]
    fn rejects_hosts_that_merely_look_local() {
        for url in [
            "https://api.openai.com/v1",
            "http://127.0.0.1.evil.com/v1",
            "http://localhost.evil.com/v1",
            "https://not-localhost.example.com/v1",
            "http://12.7.0.1:8080/v1",
            "http://127.0.0.1x:8080/v1",
            "http://[::1:8080/v1", // malformed IPv6 — never assume local
            "",
        ] {
            assert!(!is_loopback_endpoint(url), "{url} must NOT be loopback");
        }
    }

    #[test]
    fn a_loopback_profile_gets_no_total_deadline() {
        let mut p = super::tests::profile("openai", "gpt-4");
        p.base_url = "https://api.openai.com/v1".into();
        assert_eq!(total_request_timeout(&p), Some(REQUEST_TIMEOUT));

        // 8192 tokens at ~60 tok/s is over two minutes of legitimate output;
        // a total deadline would cut it mid-sentence.
        p.base_url = "http://127.0.0.1:11434/v1".into();
        assert_eq!(total_request_timeout(&p), None);
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::{parse_anthropic_stream, parse_openai_stream};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// A response whose body never sends a byte: a local model still in its
    /// prefill.
    fn silent_response() -> reqwest::Response {
        let body = reqwest::Body::wrap_stream(futures::stream::pending::<
            Result<Vec<u8>, std::io::Error>,
        >());
        reqwest::Response::from(axum::http::Response::new(body))
    }

    /// A cancel has to end the read while the server is still silent. Checked
    /// only between chunks, it waited for the next byte (a keep-alive, or the
    /// first token) while the runtime went on computing the abandoned prompt.
    #[tokio::test]
    async fn a_cancel_ends_a_silent_stream_without_waiting_for_a_byte() {
        for anthropic in [false, true] {
            let cancel = CancellationToken::new();
            let trigger = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                trigger.cancel();
            });
            let read = async {
                if anthropic {
                    parse_anthropic_stream(silent_response(), &cancel, None).await
                } else {
                    parse_openai_stream(silent_response(), &cancel, None).await
                }
            };
            let err = tokio::time::timeout(Duration::from_secs(5), read)
                .await
                .expect("the cancel did not end a silent stream")
                .unwrap_err();
            assert!(err.to_string().contains("Stream cancelled"), "{err:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn profile(provider: &str, model: &str) -> ModelProfile {
        ModelProfile {
            name: "test".into(),
            provider: provider.into(),
            model_name: model.into(),
            base_url: "https://api.example.com".into(),
            api_key: "k".into(),
            max_tokens: 8192,
            context_length: 200_000,
            adapt: None,
            vision: None,
            ..Default::default()
        }
    }

    /// A user turn shaped like a chat attachment: image first, question after —
    /// the order `ZenEngine::start_query` builds.
    fn user_turn_with_image() -> Message {
        create_user_message(vec![
            ContentBlock::Image {
                source: ImageSource {
                    source_type: "base64".into(),
                    media_type: "image/jpeg".into(),
                    data: "QUJD".into(),
                },
            },
            ContentBlock::Text {
                text: "Ảnh này là gì?".into(),
            },
        ])
    }

    #[test]
    fn openai_user_turn_carries_the_image_as_image_url() {
        let msgs = openai_messages_for_api(&[user_turn_with_image()], "").unwrap();
        let parts = msgs[0]["content"]
            .as_array()
            .expect("a turn with an image must send content parts, not a bare string");
        let img = parts
            .iter()
            .find(|p| p["type"] == "image_url")
            .expect("image part missing");
        // OpenAI-compatible endpoints want the whole data URL, mime included.
        assert_eq!(img["image_url"]["url"], "data:image/jpeg;base64,QUJD");
        assert!(parts.iter().any(|p| p["text"] == "Ảnh này là gì?"));
    }

    #[test]
    fn anthropic_user_turn_carries_the_image_as_a_base64_source() {
        let parts = anthropic_content_blocks(&user_turn_with_image().message.content);
        let img = parts
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["type"] == "image")
            .expect("image block missing");
        // Anthropic wants the raw base64 — a `data:` prefix here is a 400.
        assert_eq!(img["source"]["type"], "base64");
        assert_eq!(img["source"]["media_type"], "image/jpeg");
        assert_eq!(img["source"]["data"], "QUJD");
    }

    /// Build a request through `apply_auth` and read back its headers.
    fn auth_headers_for(profile: &ModelProfile, token: &str) -> reqwest::header::HeaderMap {
        let client = Client::new();
        let request = apply_auth(client.post("https://example.invalid"), profile, token);
        request.build().expect("request builds").headers().clone()
    }

    #[test]
    fn api_key_anthropic_profiles_keep_the_x_api_key_header() {
        let mut p = profile("anthropic", "claude-sonnet-5");
        p.adapt = Some("anthropic".into());
        let headers = auth_headers_for(&p, "sk-ant-123");

        assert_eq!(headers.get("x-api-key").unwrap(), "sk-ant-123");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert!(headers.get("authorization").is_none());
        // The OAuth-only beta flag must not leak onto API-key requests.
        assert!(headers.get("anthropic-beta").is_none());
    }

    #[test]
    fn api_key_openai_profiles_keep_the_bearer_header() {
        let mut p = profile("openai", "gpt-4o-mini");
        p.adapt = Some("openai".into());
        let headers = auth_headers_for(&p, "sk-oai-123");

        assert_eq!(headers.get("authorization").unwrap(), "Bearer sk-oai-123");
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn oauth_claude_profiles_switch_to_bearer_plus_the_oauth_beta() {
        let mut p = profile("anthropic", "claude-sonnet-5");
        p.adapt = Some("anthropic".into());
        p.oauth_provider = Some("claude".into());
        p.oauth_account_id = Some("acct-1".into());

        let headers = auth_headers_for(&p, "oauth-tok");

        assert_eq!(headers.get("authorization").unwrap(), "Bearer oauth-tok");
        assert_eq!(headers.get("anthropic-beta").unwrap(), "oauth-2025-04-20");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
        // Bearer replaces the key header rather than doubling up.
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn oauth_requests_identify_as_senclaw() {
        let mut p = profile("anthropic", "claude-sonnet-5");
        p.oauth_provider = Some("claude".into());
        let headers = auth_headers_for(&p, "t");

        let ua = headers.get("user-agent").unwrap().to_str().unwrap();
        assert!(ua.starts_with("senclaw/"), "{ua}");
        assert!(!ua.contains("claude-cli"), "{ua}");
    }

    #[test]
    fn an_app_may_only_declare_adapters_that_are_actually_routed() {
        for a in APP_DECLARABLE_ADAPTERS {
            assert!(
                ROUTED_ADAPTERS.contains(a),
                "`{a}` is offered to Space Apps but query_llm does not route it"
            );
        }
        // The in-process engines must stay out: naming one sends the turn to a
        // local engine instead of the app's port, so the app would be
        // registered and then never called.
        assert!(!APP_DECLARABLE_ADAPTERS.contains(&"local-mlx"));
        assert!(!APP_DECLARABLE_ADAPTERS.contains(&"local-candle-native"));
    }

    #[test]
    fn every_signin_provider_routes_to_a_real_adapter() {
        // The guarantee behind "Provider Sign-in": connecting an account is
        // pointless unless its wire format is actually implemented. A provider
        // whose `adapt` is not routed would silently get OpenAI-shaped
        // requests and fail with a confusing upstream parse error.
        for p in crate::providers::oauth::provider::all() {
            assert!(
                ROUTED_ADAPTERS.contains(&p.adapt),
                "provider `{}` declares adapt `{}`, which query_llm does not route",
                p.id,
                p.adapt
            );
        }
    }

    #[test]
    fn every_free_tier_preset_routes_to_a_real_adapter() {
        for p in crate::providers::all() {
            assert!(
                ROUTED_ADAPTERS.contains(&p.adapt),
                "preset `{}` declares adapt `{}`, which query_llm does not route",
                p.id,
                p.adapt
            );
        }
    }

    #[test]
    fn oauth_adapters_are_reachable_through_effective_adapter() {
        // `effective_adapter` prefers `adapt` over the provider name, so a
        // provider called e.g. "claude-oauth" must still land on its declared
        // adapter rather than being force-routed by the name match.
        for p in crate::providers::oauth::provider::all() {
            let mut prof = profile(p.id, "some-model");
            prof.adapt = Some(p.adapt.to_string());
            assert_eq!(
                effective_adapter(&prof),
                p.adapt,
                "provider `{}` misroutes",
                p.id
            );
        }
    }

    #[test]
    fn is_oauth_tracks_the_provider_field() {
        let mut p = profile("anthropic", "claude-sonnet-5");
        assert!(!p.is_oauth());
        p.oauth_provider = Some("claude".into());
        assert!(p.is_oauth());
    }

    #[test]
    fn adaptive_thinking_detection() {
        assert!(uses_adaptive_thinking(&profile(
            "anthropic",
            "claude-opus-4-6"
        )));
        assert!(uses_adaptive_thinking(&profile(
            "anthropic",
            "claude-sonnet-4-6-20260101"
        )));
        assert!(uses_adaptive_thinking(&profile(
            "anthropic",
            "claude-opus-4.8"
        )));
        assert!(uses_adaptive_thinking(&profile(
            "Anthropic",
            "Claude-Sonnet-5-0"
        )));
        // Older or non-matching models
        assert!(!uses_adaptive_thinking(&profile(
            "anthropic",
            "claude-sonnet-4-5"
        )));
        assert!(!uses_adaptive_thinking(&profile(
            "anthropic",
            "claude-3-5-sonnet-20241022"
        )));
        assert!(!uses_adaptive_thinking(&profile(
            "anthropic",
            "claude-haiku-4-6"
        )));
        // Non-anthropic provider never adaptive
        assert!(!uses_adaptive_thinking(&profile(
            "openrouter",
            "claude-opus-4-6"
        )));
    }

    #[test]
    fn max_completion_tokens_models() {
        assert!(uses_max_completion_tokens("o1-preview"));
        assert!(uses_max_completion_tokens("o3-mini"));
        assert!(uses_max_completion_tokens("o4-mini"));
        assert!(uses_max_completion_tokens("gpt-5"));
        assert!(uses_max_completion_tokens("GPT-5-turbo"));
        assert!(!uses_max_completion_tokens("gpt-4o"));
        assert!(!uses_max_completion_tokens("deepseek-chat"));
        assert!(!uses_max_completion_tokens("qwen-max"));
    }

    #[test]
    fn a_reused_tool_call_id_is_unique_across_the_transcript() {
        let mut messages = vec![
            serde_json::json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "ag_call_0", "type": "function", "function": {"name": "Skill", "arguments": "{}"}}
            ]}),
            serde_json::json!({"role": "tool", "tool_call_id": "ag_call_0", "content": "ok"}),
            serde_json::json!({"role": "assistant", "content": "now", "tool_calls": [
                {"id": "ag_call_0", "type": "function", "function": {"name": "clock_now", "arguments": "{}"}}
            ]}),
            serde_json::json!({"role": "tool", "tool_call_id": "ag_call_0", "content": "01:04"}),
        ];
        uniquify_tool_call_ids(&mut messages);
        assert_eq!(messages[0]["tool_calls"][0]["id"], "ag_call_0");
        assert_eq!(messages[1]["tool_call_id"], "ag_call_0");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "ag_call_0__2");
        assert_eq!(messages[3]["tool_call_id"], "ag_call_0__2");
    }

    #[test]
    fn a_multi_branch_union_folds_into_one_object() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "fields": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "oneOf": [
                            {"properties": {"type": {"const": "text"}, "key": {"type": "string"}}, "required": ["type", "key"]},
                            {"properties": {"type": {"const": "number"}, "min": {"type": "number"}}, "required": ["type"]}
                        ]
                    }
                },
                "note": {"anyOf": [{"type": "string"}, {"type": "null"}]}
            }
        });
        fold_gemma_unions(&mut schema);
        let items = &schema["properties"]["fields"]["items"];
        assert!(items.get("oneOf").is_none());
        assert_eq!(items["type"], "object");
        assert_eq!(items["properties"]["key"]["type"], "string");
        assert_eq!(items["properties"]["min"]["type"], "number");
        assert_eq!(items["properties"]["type"]["type"], "string");
        assert!(schema["properties"]["note"].get("anyOf").is_some(), "a nullable pair stays");
    }

    #[test]
    fn a_typeless_json_value_field_becomes_an_object() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "args": {"description": "Arguments for tool, as a JSON object"}
            }
        });
        ensure_gemma_types(&mut schema);
        assert_eq!(schema["properties"]["args"]["type"], "object");
        assert_eq!(schema["properties"]["args"]["description"], "Arguments for tool, as a JSON object");
    }

    /// Boolean subschemas (schemars output for `serde_json::Value`) must be
    /// rewritten to object schemas — Gemini-backed OpenAI proxies 400 on them.
    #[test]
    fn an_error_under_a_200_is_read_in_both_shapes() {
        use serde_json::json;
        assert_eq!(
            openai_body_error(&json!({"error": "Unexpected endpoint or method. (POST /chat/completions)"})).as_deref(),
            Some("Unexpected endpoint or method. (POST /chat/completions)")
        );
        assert_eq!(openai_body_error(&json!({"error": {"message": "bad model"}})).as_deref(), Some("bad model"));
        assert_eq!(openai_body_error(&json!({"error": null, "choices": []})), None);
        assert_eq!(openai_body_error(&json!({"choices": []})), None);
    }

    #[test]
    fn a_base_url_without_a_path_gets_the_v1_hint() {
        assert!(base_url_hint("http://192.168.0.93:1234").contains("try http://192.168.0.93:1234/v1"));
        assert!(base_url_hint("http://192.168.0.93:1234/").contains("/v1"));
        assert_eq!(base_url_hint("http://192.168.0.93:1234/v1"), "");
        assert_eq!(base_url_hint("https://openrouter.ai/api/v1"), "");
    }

    #[test]
    fn every_tool_schema_reaches_the_provider_as_an_object() {
        use serde_json::json;
        assert_eq!(object_parameters(json!({})), json!({"type": "object", "properties": {}}));
        assert_eq!(object_parameters(Value::Null), json!({"type": "object", "properties": {}}));
        assert_eq!(object_parameters(json!(true)), json!({"type": "object", "properties": {}}));
        assert_eq!(
            object_parameters(json!({"properties": {"q": {"type": "string"}}, "required": ["q"]})),
            json!({"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]})
        );
        assert_eq!(object_parameters(json!({"type": "string"})), json!({"type": "object", "properties": {}}));
        let ok = json!({"type": "object", "properties": {"a": {"type": "integer"}}});
        assert_eq!(object_parameters(ok.clone()), ok);
    }

    #[test]
    fn sanitize_schema_replaces_boolean_subschemas() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "schema": true,
                "name": {"type": "string"},
                "nested": {
                    "type": "object",
                    "properties": {"inner": true}
                },
                "list": {"type": "array", "items": true},
                "choice": {"anyOf": [true, {"type": "string"}]}
            },
            "required": ["schema"]
        });
        sanitize_schema_node(&mut schema);
        let props = &schema["properties"];
        assert_eq!(props["schema"], serde_json::json!({"type": "object"}));
        assert_eq!(props["name"], serde_json::json!({"type": "string"}));
        assert_eq!(
            props["nested"]["properties"]["inner"],
            serde_json::json!({"type": "object"})
        );
        assert_eq!(
            props["list"]["items"],
            serde_json::json!({"type": "object"})
        );
        assert_eq!(
            props["choice"]["anyOf"],
            serde_json::json!([{"type": "object"}, {"type": "string"}])
        );
        // Non-schema fields untouched.
        assert_eq!(schema["required"], serde_json::json!(["schema"]));
    }

    /// `build_openai_tools` must never emit a boolean in a `properties` map.
    #[test]
    fn build_openai_tools_sanitizes_value_typed_params() {
        struct AnyTool;
        #[async_trait::async_trait]
        impl Tool for AnyTool {
            fn name(&self) -> &str {
                "any_tool"
            }
            fn description(&self) -> &str {
                "test"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({
                    "type": "object",
                    "properties": {"payload": true}
                })
            }
            fn is_read_only(&self) -> bool {
                true
            }
            async fn call(
                &self,
                _input: Value,
                _ctx: &ToolContext<'_>,
            ) -> anyhow::Result<Vec<ToolOutput>> {
                unreachable!()
            }
            fn gen_tool_result_message(
                &self,
                _data: &Value,
                _input: &Value,
            ) -> crate::zen_core::ToolResultMessage {
                unreachable!()
            }
            fn get_display_title(&self, _input: &Value) -> String {
                "any_tool".into()
            }
        }
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(AnyTool)];
        let built = build_openai_tools(&tools);
        assert_eq!(
            built[0]["function"]["parameters"]["properties"]["payload"],
            serde_json::json!({"type": "object"})
        );
    }


    #[test]
    fn resolve_adapter_detects_anthropic() {
        assert_eq!(resolve_adapter("anthropic"), "anthropic");
        assert_eq!(resolve_adapter("claude"), "anthropic");
        assert_eq!(resolve_adapter("Anthropic"), "anthropic");
    }

    #[test]
    fn resolve_adapter_defaults_to_openai() {
        assert_eq!(resolve_adapter("openai"), "openai");
        assert_eq!(resolve_adapter("openrouter"), "openai");
        assert_eq!(resolve_adapter("unknown"), "openai");
    }

    #[test]
    fn error_classify_cancelled() {
        let err = anyhow::anyhow!("Request cancelled");
        let classified = LlmError::classify(&err);
        assert!(!classified.should_emit());
    }

    #[test]
    fn error_classify_context_length() {
        let err = anyhow::anyhow!("context_length_exceeded: maximum context length");
        let classified = LlmError::classify(&err);
        assert!(classified.is_context_length);
        assert_eq!(classified.code, "CONTEXT_TOO_LONG");
    }

    #[test]
    fn error_classify_auth() {
        let err = anyhow::anyhow!("HTTP 401 Unauthorized");
        let classified = LlmError::classify(&err);
        assert_eq!(classified.code, "AUTH_ERROR");
    }

    #[test]
    fn error_classify_auth_not_triggered_by_authors_path_or_bare_401() {
        let err = anyhow::anyhow!(
            "chat template apply failed: /Users/x/docs/authors/guide.md:12:5 error"
        );
        let c = LlmError::classify(&err);
        assert_ne!(c.code, "AUTH_ERROR", "expected not AUTH_ERROR: {}", c.code);

        let err2 = anyhow::anyhow!("mlx forward failed: shape [32, 401, 128] mismatch");
        let c2 = LlmError::classify(&err2);
        assert_ne!(c2.code, "AUTH_ERROR");
    }

    #[test]
    fn error_classify_network_not_triggered_by_mcp_tool_json_noise() {
        let err = anyhow::anyhow!(
            "render failed: {{\n  \"tools\": [{{\n    \"timeout_ms\": 30000,\n    \"description\": \"connection state\"\n  }}]\n}}"
        );
        let c = LlmError::classify(&err);
        assert_ne!(c.code, "NETWORK_ERROR");

        let err2 =
            anyhow::anyhow!("https://json-schema.org/draft/2020-12/schema parse error in tool");
        let c2 = LlmError::classify(&err2);
        assert_ne!(c2.code, "API_RESPONSE_ERROR");
    }

    #[test]
    fn error_classify_network_still_detects_connection_refused() {
        let err = anyhow::anyhow!("error sending request: connection refused (os error 61)");
        let c = LlmError::classify(&err);
        assert_eq!(c.code, "NETWORK_ERROR");
    }

    #[test]
    fn error_classify_rate_limit() {
        let err = anyhow::anyhow!("429 rate limit exceeded");
        let classified = LlmError::classify(&err);
        assert_eq!(classified.code, "RATE_LIMIT");
    }

    #[test]
    fn extract_http_status_finds_code() {
        assert_eq!(extract_http_status("API error (429)"), Some(429));
        assert_eq!(extract_http_status("error (500) internal"), Some(500));
        assert_eq!(extract_http_status("no status here"), None);
    }


    #[test]
    fn openai_messages_expand_tool_use_and_tool_results() {
        let msgs = vec![
            create_user_message(vec![ContentBlock::Text {
                text: "read project".into(),
            }]),
            Message {
                msg_type: "assistant".into(),
                message: MessagePayload {
                    role: "assistant".into(),
                    content: vec![ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"path": "/tmp/x"}),
                    }],
                },
                uuid: "a1".into(),
                usage: None,
            },
            create_user_message(vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "file contents".into(),
                is_error: false,
            }]),
        ];
        let out = openai_messages_for_api(&msgs, "You are helpful.").unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[1]["content"], "read project");
        assert_eq!(out[2]["role"], "assistant");
        assert!(out[2]["content"].is_null());
        assert!(out[2]["tool_calls"].is_array());
        assert_eq!(out[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(out[3]["role"], "tool");
        assert_eq!(out[3]["tool_call_id"], "call_1");
        assert_eq!(out[3]["content"], "file contents");
    }

    #[test]
    fn openai_messages_include_reasoning_content_for_thinking() {
        let msgs = vec![Message {
            msg_type: "assistant".into(),
            message: MessagePayload {
                role: "assistant".into(),
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "step by step...".into(),
                    },
                    ContentBlock::Text {
                        text: "Hello".into(),
                    },
                ],
            },
            uuid: "a1".into(),
            usage: None,
        }];
        let out = openai_messages_for_api(&msgs, "").unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[0]["content"], "Hello");
        assert_eq!(out[0]["reasoning_content"], "step by step...");
    }
}

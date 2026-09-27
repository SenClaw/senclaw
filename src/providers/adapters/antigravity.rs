//! Antigravity adapter — Google Code Assist.
//!
//! Three things make this different from every other adapter here:
//!
//! 1. **Gemini content shape.** Messages are `contents[{role, parts[]}]` with
//!    `model` (not `assistant`) as the reply role, and tool traffic rides in
//!    `functionCall` / `functionResponse` parts rather than dedicated blocks.
//! 2. **An envelope.** The Gemini request is nested under `request`, wrapped
//!    with a Code Assist project id and a request id.
//! 3. **Schema pickiness.** Gemini rejects most JSON Schema keywords, so tool
//!    parameters have to be stripped down before they are sent.
//! 4. **Two hosts.** Completions go to `daily-cloudcode-pa.googleapis.com`;
//!    project discovery only works on `cloudcode-pa.googleapis.com`. Pointing
//!    either at the other's host fails — prod rejects a generated project with
//!    `CONSUMER_INVALID`, and daily 404s the discovery endpoints.
//!
//! Ported from 9router's `open-sse/executors/antigravity.js` and its Gemini
//! schema cleaner.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use reqwest::Client;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::zen_core::query_llm::{build_assistant_message, post_authed};
use crate::zen_core::{ContentBlock, Message, ModelProfile, RawUsage, Tool};

/// Google caps Code Assist output well below what some models advertise.
const MAX_OUTPUT_TOKENS: u32 = 64_000;

/// JSON Schema keywords Gemini rejects outright. Sending any of them fails the
/// whole request, so they are removed at every level of the schema.
const UNSUPPORTED_SCHEMA_KEYWORDS: &[&str] = &[
    "minLength",
    "maxLength",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minItems",
    "maxItems",
    "format",
    "default",
    "examples",
    "$schema",
    "$defs",
    "definitions",
    "const",
    "$ref",
    "$comment",
    "deprecated",
    "readOnly",
    "writeOnly",
    "additionalProperties",
    "propertyNames",
    "patternProperties",
    "enumDescriptions",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "dependencies",
    "dependentSchemas",
    "dependentRequired",
    "title",
    "optional",
    "if",
    "then",
    "else",
    "contentMediaType",
    "contentEncoding",
];

/// Placeholder `thoughtSignature` for replayed function calls.
///
/// Gemini 3+ rejects a `functionCall` part that carries no `thoughtSignature`:
///
/// > Function call is missing a thought_signature in functionCall parts. This
/// > is required for tools to work correctly…
///
/// The signature is an opaque token Google mints alongside a live tool call.
/// SenClaw's message history does not persist it — nor does any other client,
/// including Google's own IDE — so a replayed conversation has nothing to send.
/// Google's clients backfill this known-good constant instead, which the API
/// accepts for continuity while contributing no reasoning of its own.
const DEFAULT_THOUGHT_SIGNATURE: &str = "EuwGCukGAXLI2nxwZIq54WWSoL/YN0P3TsDZ7zRnLi8g0S4aVr2HUGxvaHKySuY6HAVzcE0GPGjXrytLIldxthSvfxgUlJh6Qa9Z+Oj5QZBlYdg6HaJ6yuY5R7waE6rdwBsRf7Ft2j3DJ9rMi9qhWFqApewYtPhls3VHtuvND3l8Rm09+lbAXQs6KKWEWrxNLKTBkfpMgXhRERc/TQRMZu1twAablm6/Zk1tsYRvfWKLsNbeKF+CCojJdXJKvnR/8Ouuoa+Y2Ti20hcW7aZIIjZDFYPU//k6Ybmhg69J/imbFai2ckhfLaisqdDkdoIiBJScTOUvYqP6AE9d4MsydSC+UlhIMk4hoP76R8vUSCZRMkjOaDXstf/QoVZKbt94wyRZgAJ1G0BqI8L5ow86kLpA4wJEtxsRGymOE4bKUvApveBakYDNM9APkf+LbtbzWSseGjoZcSlycF9iN8Q2XNYKRrHbv3Lr5Y8JjdH/5y/6SHkNehTEZugaeGnSPSyCTWto1kQgHpxdWmhkLfJGNUGLmue7Mesj4TSms4J33mRpYVhNB/J333FCqIP0hr/E7BkkjEn7yZ4X7SQlh+xKPurapsnHRwiKmtsilmEFrnTE9iQr+pMr6M29qqFNv1tr5yumbaJw8JW9sB15tNsRv+dW6BjNanbsKz7HCgKUBc8tGy+7YuhXzAfViyRefcjK7eZW0Fbyt7AbybJTKz78W8NH7ye6LAwzOebXpeZ4D43fNIt8bKh26qgduSQv/7o+pAflkuqHZ99YWgHQ8h8OkZFi3eOiSYjsjhdZ/czWOdoPI/OnqIldzMPF5YlrKBLFX8VhRKVmqgsmWf5PHGulHhMkVlS+XG2UIseGy69ARa93D78Gsa+1n1kJr7EEB7Rh+27vUMxVYLdz1yMSvE5nalTAlg/ZeG8+XQ0cHuAI3KbQpHW2Q++RdXfm5JzD5WdJZUU+Zn8t8UUn85BH4RxZLeE0qJikgSsKoYVBc6YhiMjhPgkR95ReimY4Z0xCJdRo1gjexOFeODZMpQF6Yxnoic7IrdgsFA3iePTbFnPp3IAM1fAThWhXJUn3QInUOTd5o1qmTmn6REbL15g/JQNl+dqUoPkhleeb2V3kjqp1okmO3wMZbPknR3S1LZNmlS72/iBQUm+n2b/RCn4PjmM2";

/// Send one turn to Code Assist.
pub async fn query_antigravity(
    client: &Client,
    messages: &[Message],
    system_prompt: &str,
    tools: &[Arc<dyn Tool>],
    cancel: &CancellationToken,
    profile: &ModelProfile,
) -> Result<Message> {
    let project_id = resolve_project_id(client, profile).await?;
    let session_id = stable_id(&format!(
        "session:{}:{}",
        profile.oauth_account_id.as_deref().unwrap_or("anon"),
        profile.model_name
    ));

    let mut request = Map::new();
    request.insert("contents".into(), Value::Array(gemini_contents(messages)));
    if !system_prompt.is_empty() {
        request.insert(
            "systemInstruction".into(),
            serde_json::json!({ "parts": [{ "text": system_prompt }] }),
        );
    }
    request.insert(
        "generationConfig".into(),
        serde_json::json!({
            "maxOutputTokens": profile.max_tokens.min(MAX_OUTPUT_TOKENS),
        }),
    );
    if !tools.is_empty() {
        request.insert(
            "tools".into(),
            serde_json::json!([{ "functionDeclarations": gemini_tools(tools) }]),
        );
        // VALIDATED makes Gemini check calls against the declarations rather
        // than emitting free-form ones we then fail to route.
        request.insert(
            "toolConfig".into(),
            serde_json::json!({ "functionCallingConfig": { "mode": "VALIDATED" } }),
        );
    }
    request.insert("sessionId".into(), Value::String(session_id.clone()));

    let body = serde_json::json!({
        "project": project_id,
        "model": profile.model_name,
        "userAgent": "antigravity",
        "requestType": "agent",
        "requestId": build_request_id(&session_id, &profile.model_name, messages.len()),
        "request": Value::Object(request),
    });

    let url = format!(
        "{}/v1internal:streamGenerateContent?alt=sse",
        profile.base_url.trim_end_matches('/')
    );

    if cancel.is_cancelled() {
        bail!("Request cancelled before send");
    }

    debug!("[antigravity] POST {url}");
    let response = post_authed(client, &url, profile, &body)
        .await
        .context("Antigravity request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        if is_project_rejected(status.as_u16(), &text) {
            // The cached project is not one this account may bill against.
            // Forget it so the next attempt rediscovers, and say what to do
            // rather than echoing Google's wall of JSON.
            if let Some(account_id) = profile.oauth_account_id.as_deref() {
                forget_cached_project(account_id);
            }
            bail!(
                "Google rejected the Code Assist project `{project_id}` for this account \
                 (403 {}). The cached project has been cleared, so the next message will \
                 look it up again. If this repeats, the account may not be entitled to \
                 Antigravity.",
                status.canonical_reason().unwrap_or("Forbidden")
            );
        }

        bail!("Antigravity API error ({status}): {text}");
    }

    parse_stream(response, cancel).await
}

/// How many times to poll `onboardUser` before giving up.
const ONBOARD_MAX_ATTEMPTS: usize = 10;
/// Google's onboarding operation is asynchronous; this is the gap between polls.
const ONBOARD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// `cloudaicompanionProject` comes back either as a bare string or as an
/// object carrying an `id`. Normalise both, treating blanks as absent.
pub(crate) fn extract_project(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let raw = match value {
        Value::String(s) => s.trim().to_string(),
        Value::Object(_) => value.get("id")?.as_str()?.trim().to_string(),
        _ => return None,
    };
    (!raw.is_empty()).then_some(raw)
}

/// The default tier from a `loadCodeAssist` reply, used to onboard an account
/// that has no project yet.
pub(crate) fn extract_default_tier(json: &Value) -> String {
    json.get("allowedTiers")
        .and_then(|t| t.as_array())
        .and_then(|tiers| {
            tiers
                .iter()
                .find(|t| t.get("isDefault").and_then(|d| d.as_bool()) == Some(true))
                .and_then(|t| t.get("id"))
                .and_then(|id| id.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        // Google's own fallback when a project predates tiering.
        .unwrap_or_else(|| "legacy-tier".to_string())
}

/// A project label for an account that has no Cloud project of its own.
///
/// The completion host (`daily-…`) treats `project` as a routing label and does
/// not resolve it against Cloud Resource Manager, so a generated name works —
/// which is how Antigravity serves free-tier users who never created a project.
/// The *prod* host does resolve it, and answers `CONSUMER_INVALID`; that
/// difference is why discovery and completions must not share a base URL.
///
/// Derived from the account id so a conversation keeps one label across turns.
fn synthetic_project_id(seed: &str) -> String {
    const ADJECTIVES: [&str; 5] = ["useful", "bright", "swift", "calm", "bold"];
    const NOUNS: [&str; 5] = ["fuze", "wave", "spark", "flow", "core"];
    let digest = Sha256::digest(seed.as_bytes());
    let adj = ADJECTIVES[digest[0] as usize % ADJECTIVES.len()];
    let noun = NOUNS[digest[1] as usize % NOUNS.len()];
    format!("{adj}-{noun}-{}", hex_encode(&digest[2..5]))
}

/// Headers Code Assist's discovery endpoints expect. Unlike the completion
/// call these are part of the API contract — the endpoint 400s without the
/// client-metadata header.
fn discovery_headers(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {token}")),
        ("Content-Type", "application/json".into()),
        (
            "Client-Metadata",
            crate::providers::oauth::transport::code_assist_client_metadata().to_string(),
        ),
        // Discovery is issued by the Google API client library, not the IDE, so
        // it carries a different identity than the completion call. See the
        // note on these constants in `oauth::transport`.
        (
            "User-Agent",
            crate::providers::oauth::transport::CODE_ASSIST_DISCOVERY_USER_AGENT.to_string(),
        ),
        (
            "X-Goog-Api-Client",
            crate::providers::oauth::transport::CODE_ASSIST_DISCOVERY_API_CLIENT.to_string(),
        ),
    ]
}

/// Compact a JSON value for an error message.
fn truncate_json(value: &Value, max: usize) -> String {
    let text = value.to_string();
    if text.chars().count() <= max {
        return text;
    }
    text.chars().take(max).collect::<String>() + "…"
}

/// The Code Assist project this account should bill against.
///
/// Three steps, mirroring Google's own clients:
/// 1. `loadCodeAssist` — an account that already has a project reports it here.
/// 2. `onboardUser` — a fresh account has none, and must be onboarded onto the
///    default tier first. The operation is asynchronous, so this polls.
/// 3. [`synthetic_project_id`] — free-tier accounts complete onboarding without
///    ever being issued a Cloud project. The completion host accepts a label,
///    so a generated one keeps them working instead of failing the sign-in.
///
/// Discovery in steps 1–2 must hit the *prod* host even though completions go
/// to `daily-`; the two are not interchangeable.
///
/// The result is cached on the account: it never changes, and steps 1–2 are
/// several round trips.
async fn resolve_project_id(client: &Client, profile: &ModelProfile) -> Result<String> {
    let Some(account_id) = profile.oauth_account_id.as_deref() else {
        bail!("Antigravity needs an OAuth account; this model config has none");
    };
    let Some(manager) = crate::providers::oauth::global() else {
        bail!("OAuth subsystem is not initialised");
    };
    let account = manager
        .account(account_id)
        .ok_or_else(|| anyhow::anyhow!("OAuth account `{account_id}` is gone"))?;

    if let Some(cached) = crate::providers::oauth::transport::cached_project_id(&account) {
        return Ok(cached);
    }

    let token = &account.access_token;

    // Discovery is best-effort: a free-tier account is entitled to chat even
    // when it may not call these endpoints, so a failure here must not block
    // the conversation — it just means falling through to a generated label.
    let load = post_discovery(
        client,
        crate::providers::oauth::provider::ANTIGRAVITY_LOAD_CODE_ASSIST_URL,
        token,
        &serde_json::json!({
            "metadata": crate::providers::oauth::transport::code_assist_client_metadata(),
        }),
    )
    .await
    .unwrap_or_else(|e| {
        debug!("[antigravity] loadCodeAssist unavailable ({e}); continuing");
        Value::Null
    });

    // An account that already has a project reports it here; that answer wins.
    // Onboarding only runs to *create* one, and its reply is the next choice.
    let discovered = extract_project(load.get("cloudaicompanionProject"));
    let project = match discovered {
        Some(p) => p,
        None => {
            let tier = extract_default_tier(&load);
            debug!("[antigravity] no project on the account; onboarding onto tier {tier}");
            match onboard_user(client, token, &tier).await {
                Ok(Some(p)) => p,
                Ok(None) | Err(_) => {
                    let generated = synthetic_project_id(account_id);
                    debug!("[antigravity] using generated project label {generated}");
                    generated
                }
            }
        }
    };

    let _ = manager.set_extra(
        account_id,
        crate::providers::oauth::transport::ANTIGRAVITY_PROJECT_KEY,
        Value::String(project.clone()),
    );
    Ok(project)
}

/// Drop a cached project id so the next call rediscovers it.
///
/// Called when Google rejects the project we hold: a stale or invalid id must
/// not be retried forever, and re-running discovery is cheap next to a chat
/// that fails every time.
fn forget_cached_project(account_id: &str) {
    if let Some(manager) = crate::providers::oauth::global() {
        let _ = manager.set_extra(
            account_id,
            crate::providers::oauth::transport::ANTIGRAVITY_PROJECT_KEY,
            Value::String(String::new()),
        );
    }
}

/// True when a Code Assist error says the project itself is unusable, as
/// opposed to a transient or auth failure.
pub(crate) fn is_project_rejected(status: u16, body: &str) -> bool {
    status == 403
        && (body.contains("CONSUMER_INVALID")
            || body.contains("PERMISSION_DENIED")
            || body.contains("Permission denied on resource project"))
}

/// Drive `onboardUser` until the operation reports `done`.
async fn onboard_user(client: &Client, token: &str, tier: &str) -> Result<Option<String>> {
    let body = serde_json::json!({
        "tierId": tier,
        "metadata": crate::providers::oauth::transport::code_assist_client_metadata(),
    });

    let mut last = Value::Null;
    for attempt in 0..ONBOARD_MAX_ATTEMPTS {
        let result = post_discovery(
            client,
            crate::providers::oauth::provider::ANTIGRAVITY_ONBOARD_USER_URL,
            token,
            &body,
        )
        .await
        .context("Code Assist onboarding")?;

        if result.get("done").and_then(|d| d.as_bool()) == Some(true) {
            // The project may sit under `response` (long-running-operation
            // shape) or at the top level, depending on how far along the
            // operation was when it completed.
            return Ok(extract_project(
                result
                    .get("response")
                    .and_then(|r| r.get("cloudaicompanionProject")),
            )
            .or_else(|| extract_project(result.get("cloudaicompanionProject"))));
        }
        last = result;

        if attempt + 1 < ONBOARD_MAX_ATTEMPTS {
            tokio::time::sleep(ONBOARD_POLL_INTERVAL).await;
        }
    }

    bail!(
        "Code Assist onboarding did not finish after {} attempts. Last reply: {}",
        ONBOARD_MAX_ATTEMPTS,
        truncate_json(&last, 300)
    )
}

async fn post_discovery(
    client: &Client,
    url: &str,
    token: &str,
    body: &Value,
) -> Result<Value> {
    let mut request = client.post(url).json(body);
    for (name, value) in discovery_headers(token) {
        request = request.header(name, value);
    }

    let response = request.send().await.context("request failed")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("({status}): {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("parse response: {text}"))
}

/// Deterministic hex id from a seed, so a conversation keeps the same session
/// and request lineage across turns.
fn stable_id(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let hex = hex_encode(&digest[..16]);
    format!(
        "{}-{}-4{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The IDE-shaped request id Code Assist expects:
/// `agent/<conversation>/<step-ordinal>/<trajectory>/<step>`.
fn build_request_id(session_id: &str, model: &str, message_count: usize) -> String {
    let conversation = stable_id(&format!("conversation:{session_id}"));
    let trajectory = stable_id(&format!("trajectory:{session_id}:{model}"));
    let step = (message_count * 2).max(1);
    format!("agent/{conversation}/{step}/{trajectory}/{step}")
}

/// SenClaw messages → Gemini `contents`.
pub(crate) fn gemini_contents(messages: &[Message]) -> Vec<Value> {
    let mut contents: Vec<Value> = Vec::new();

    for msg in messages {
        let mut parts: Vec<Value> = Vec::new();
        // Gemini calls the assistant "model", and a turn carrying a tool
        // result must be attributed to the user regardless of who produced it.
        let mut role = if msg.message.role == "assistant" {
            "model"
        } else {
            "user"
        };

        for block in &msg.message.content {
            match block {
                ContentBlock::Text { text } => {
                    if !text.is_empty() {
                        parts.push(serde_json::json!({ "text": text }));
                    }
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    parts.push(serde_json::json!({
                        "functionCall": {
                            "name": sanitize_function_name(name),
                            "args": if input.is_null() {
                                Value::Object(Map::new())
                            } else {
                                input.clone()
                            },
                        },
                        // Required by Gemini 3+; see DEFAULT_THOUGHT_SIGNATURE.
                        "thoughtSignature": DEFAULT_THOUGHT_SIGNATURE,
                    }));
                }
                ContentBlock::ToolResult { content, .. } => {
                    role = "user";
                    parts.push(serde_json::json!({
                        "functionResponse": {
                            // Gemini keys the response by tool name; SenClaw
                            // tracks tool_use_id, so the name is recovered from
                            // the preceding call below.
                            "name": "tool",
                            "response": { "output": content },
                        }
                    }));
                }
                ContentBlock::Image { source } => {
                    parts.push(serde_json::json!({
                        "inlineData": {
                            "mimeType": source.media_type,
                            "data": source.data,
                        }
                    }));
                }
                // Gemini rejects replayed thought parts.
                ContentBlock::Thinking { .. } => {}
                ContentBlock::ControlSignal { .. } => {}
            }
        }

        if !parts.is_empty() {
            contents.push(serde_json::json!({ "role": role, "parts": parts }));
        }
    }

    // Name each functionResponse after the call it answers: Gemini matches on
    // name, and an unmatched response aborts the turn.
    name_function_responses(&mut contents);
    contents
}

/// Walk the built contents and copy each `functionCall` name onto the
/// `functionResponse` that follows it, in order.
fn name_function_responses(contents: &mut [Value]) {
    let mut pending: Vec<String> = Vec::new();

    for content in contents.iter_mut() {
        let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) else {
            continue;
        };
        for part in parts.iter_mut() {
            if let Some(name) = part
                .get("functionCall")
                .and_then(|c| c.get("name"))
                .and_then(|n| n.as_str())
            {
                pending.push(name.to_string());
                continue;
            }
            if part.get("functionResponse").is_some() && !pending.is_empty() {
                let name = pending.remove(0);
                if let Some(resp) = part.get_mut("functionResponse") {
                    resp["name"] = Value::String(name);
                }
            }
        }
    }
}

/// Gemini requires `[a-zA-Z_][a-zA-Z0-9_.:-]{0,63}`.
pub(crate) fn sanitize_function_name(name: &str) -> String {
    if name.is_empty() {
        return "_unknown".to_string();
    }
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !out.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        out.insert(0, '_');
    }
    out.chars().take(64).collect()
}

/// Tool declarations with schemas Gemini will accept.
pub(crate) fn gemini_tools(tools: &[Arc<dyn Tool>]) -> Vec<Value> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();

    for tool in tools {
        let name = sanitize_function_name(tool.name());
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());

        let mut schema = tool.input_schema();
        sanitize_schema(&mut schema);
        // Gemini rejects a function with no parameter object at all.
        if !schema.is_object() {
            schema = serde_json::json!({ "type": "object", "properties": {} });
        }

        out.push(serde_json::json!({
            "name": name,
            "description": tool.description(),
            "parameters": schema,
        }));
    }

    out
}

/// Make a JSON Schema acceptable to Gemini, in place.
///
/// Gemini's parameter schema is a protobuf message, not JSON Schema, so it
/// rejects far more than it accepts. Removing unknown keywords is not enough:
/// constructs like `type: ["string", "null"]` or `anyOf` have to be *collapsed*
/// into the single-shape equivalent first, or the proto parser fails with
/// "Proto field is not repeating, cannot start list".
///
/// Order matters. Each pass assumes the previous ones have run:
/// const → enum → merge allOf → collapse anyOf/oneOf → collapse type arrays →
/// infer object type → drop unknown keywords → prune `required`.
pub(crate) fn sanitize_schema(schema: &mut Value) {
    convert_const_to_enum(schema);
    convert_enum_values_to_strings(schema);
    merge_all_of(schema);
    flatten_any_of_one_of(schema);
    flatten_type_arrays(schema);
    ensure_object_type(schema);
    remove_unsupported_keywords(schema);
    cleanup_required(schema);
}

/// Walk every nested object/array value, applying `f` to each object node.
fn walk(schema: &mut Value, f: &mut impl FnMut(&mut Map<String, Value>)) {
    match schema {
        Value::Object(map) => {
            f(map);
            for value in map.values_mut() {
                walk(value, f);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, f);
            }
        }
        _ => {}
    }
}

/// `const: X` → `enum: [X]`. Gemini has no `const`.
fn convert_const_to_enum(schema: &mut Value) {
    walk(schema, &mut |map| {
        if map.contains_key("const") && !map.contains_key("enum") {
            if let Some(value) = map.remove("const") {
                map.insert("enum".into(), Value::Array(vec![value]));
            }
        }
    });
}

/// Gemini's enum is a repeated string, and it requires `type: "string"`
/// alongside — a numeric enum without it is a 400.
fn convert_enum_values_to_strings(schema: &mut Value) {
    walk(schema, &mut |map| {
        let Some(Value::Array(values)) = map.get("enum") else {
            return;
        };
        let as_strings: Vec<Value> = values
            .iter()
            .map(|v| match v {
                Value::String(s) => Value::String(s.clone()),
                other => Value::String(other.to_string().trim_matches('"').to_string()),
            })
            .collect();
        map.insert("enum".into(), Value::Array(as_strings));
        map.entry("type")
            .or_insert_with(|| Value::String("string".into()));
    });
}

/// Fold `allOf` branches into the parent: Gemini has no composition keyword,
/// and the union of properties is the closest faithful rendering.
fn merge_all_of(schema: &mut Value) {
    walk(schema, &mut |map| {
        let Some(Value::Array(branches)) = map.remove("allOf") else {
            return;
        };

        let mut properties = map
            .get("properties")
            .and_then(|p| p.as_object().cloned())
            .unwrap_or_default();
        let mut required: Vec<Value> = map
            .get("required")
            .and_then(|r| r.as_array().cloned())
            .unwrap_or_default();

        for branch in branches {
            if let Some(props) = branch.get("properties").and_then(|p| p.as_object()) {
                for (k, v) in props {
                    properties.insert(k.clone(), v.clone());
                }
            }
            if let Some(reqs) = branch.get("required").and_then(|r| r.as_array()) {
                for req in reqs {
                    if !required.contains(req) {
                        required.push(req.clone());
                    }
                }
            }
        }

        if !properties.is_empty() {
            map.insert("properties".into(), Value::Object(properties));
        }
        if !required.is_empty() {
            map.insert("required".into(), Value::Array(required));
        }
    });
}

/// Score a candidate branch: richer shapes win, so `anyOf` collapses onto the
/// most informative alternative rather than an arbitrary one.
fn branch_score(schema: &Value) -> u8 {
    let ty = schema.get("type").and_then(|t| t.as_str());
    if ty == Some("object") || schema.get("properties").is_some() {
        3
    } else if ty == Some("array") || schema.get("items").is_some() {
        2
    } else if ty.is_some_and(|t| t != "null") {
        1
    } else {
        0
    }
}

/// Collapse `anyOf`/`oneOf` onto their best non-null branch.
fn flatten_any_of_one_of(schema: &mut Value) {
    walk(schema, &mut |map| {
        for key in ["anyOf", "oneOf"] {
            let Some(Value::Array(branches)) = map.remove(key) else {
                continue;
            };
            let best = branches
                .into_iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) != Some("null"))
                .max_by_key(branch_score);
            if let Some(Value::Object(chosen)) = best {
                for (k, v) in chosen {
                    map.insert(k, v);
                }
            }
        }
    });
}

/// `type: ["string", "null"]` → `type: "string"`.
///
/// This is the pass whose absence produced "Proto field is not repeating,
/// cannot start list": Gemini's `type` is a scalar enum, never a list.
fn flatten_type_arrays(schema: &mut Value) {
    walk(schema, &mut |map| {
        let Some(Value::Array(types)) = map.get("type") else {
            return;
        };
        let first = types
            .iter()
            .filter_map(|t| t.as_str())
            .find(|t| *t != "null")
            .unwrap_or("string")
            .to_string();
        map.insert("type".into(), Value::String(first));
    });
}

/// A node with `properties` but no `type` is rejected; Gemini infers nothing.
fn ensure_object_type(schema: &mut Value) {
    walk(schema, &mut |map| {
        if map.contains_key("properties") && !map.contains_key("type") {
            map.insert("type".into(), Value::String("object".into()));
        }
    });
}

fn remove_unsupported_keywords(schema: &mut Value) {
    walk(schema, &mut |map| {
        for key in UNSUPPORTED_SCHEMA_KEYWORDS {
            map.remove(*key);
        }
    });
}

/// `required` may only name properties that survived the strip.
fn cleanup_required(schema: &mut Value) {
    walk(schema, &mut |map| {
        let Some(Value::Array(required)) = map.get("required").cloned() else {
            return;
        };
        let Some(props) = map.get("properties").and_then(|p| p.as_object()) else {
            // Nothing to validate against — a bare `required` is meaningless.
            map.remove("required");
            return;
        };
        let kept: Vec<Value> = required
            .into_iter()
            .filter(|r| r.as_str().is_some_and(|s| props.contains_key(s)))
            .collect();
        if kept.is_empty() {
            map.remove("required");
        } else {
            map.insert("required".into(), Value::Array(kept));
        }
    });
}

/// True when `schema` still contains anything Gemini's proto parser rejects.
/// Used by tests as a single acceptance check.
#[cfg(test)]
fn gemini_would_reject(schema: &Value) -> Option<String> {
    match schema {
        Value::Object(map) => {
            if map.get("type").is_some_and(|t| t.is_array()) {
                return Some(format!("type is a list: {}", map["type"]));
            }
            for key in UNSUPPORTED_SCHEMA_KEYWORDS {
                if map.contains_key(*key) {
                    return Some(format!("unsupported keyword `{key}`"));
                }
            }
            if map.contains_key("properties") && !map.contains_key("type") {
                return Some("properties without a type".into());
            }
            map.values().find_map(gemini_would_reject)
        }
        Value::Array(items) => items.iter().find_map(gemini_would_reject),
        _ => None,
    }
}

/// Folds the Gemini SSE stream into an assistant message.
#[derive(Default)]
pub(crate) struct GeminiAccumulator {
    pub text: String,
    pub reasoning: String,
    calls: Vec<Value>,
    pub usage: Option<RawUsage>,
}

impl GeminiAccumulator {
    /// Apply one `data:` payload. Code Assist nests the Gemini reply under
    /// `response`; plain Gemini does not, so both shapes are accepted.
    pub(crate) fn apply(&mut self, event: &Value) {
        let body = event.get("response").unwrap_or(event);

        if let Some(parts) = body
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(call) = part.get("functionCall") {
                    self.calls.push(serde_json::json!({
                        // Gemini does not issue call ids; synthesise a stable
                        // one so the agent loop can match the result back.
                        "id": format!("ag_call_{}", self.calls.len()),
                        "name": call.get("name").and_then(|n| n.as_str()).unwrap_or_default(),
                        "input": call.get("args").cloned().unwrap_or_else(|| Value::Object(Map::new())),
                    }));
                    continue;
                }
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    // A part flagged `thought` is reasoning, not output.
                    if part.get("thought").and_then(|t| t.as_bool()) == Some(true) {
                        self.reasoning.push_str(text);
                    } else {
                        self.text.push_str(text);
                    }
                }
            }
        }

        if let Some(usage) = body.get("usageMetadata") {
            if !usage.is_null() {
                self.usage = RawUsage::from_json(&serde_json::json!({
                    "input_tokens": usage.get("promptTokenCount").and_then(|v| v.as_u64()),
                    "output_tokens": usage.get("candidatesTokenCount").and_then(|v| v.as_u64()),
                }));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn tool_calls(&self) -> &[Value] {
        &self.calls
    }

    pub(crate) fn into_message(self) -> Result<Message> {
        let calls = self.calls.clone();
        build_assistant_message(&self.text, &self.reasoning, &calls, self.usage)
    }
}

async fn parse_stream(response: reqwest::Response, cancel: &CancellationToken) -> Result<Message> {
    let mut stream = response.bytes_stream();
    let mut acc = GeminiAccumulator::default();
    let mut pending = String::new();

    while let Some(chunk) = stream.next().await {
        if cancel.is_cancelled() {
            bail!("Stream cancelled");
        }
        let chunk = chunk.context("Antigravity stream chunk error")?;
        pending.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(idx) = pending.find('\n') {
            let line = pending[..idx].trim().to_string();
            pending.drain(..=idx);
            if let Some(event) = parse_sse_line(&line) {
                acc.apply(&event);
            }
        }
    }

    if let Some(event) = parse_sse_line(pending.trim()) {
        acc.apply(&event);
    }

    acc.into_message()
}

/// Extract the JSON payload from one SSE line.
pub(crate) fn parse_sse_line(line: &str) -> Option<Value> {
    let payload = line.strip_prefix("data:")?.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    serde_json::from_str(payload).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::{ImageSource, MessagePayload};

    fn msg(role: &str, content: Vec<ContentBlock>) -> Message {
        Message {
            msg_type: role.to_string(),
            message: MessagePayload {
                role: role.to_string(),
                content,
            },
            uuid: "u".into(),
            usage: None,
        }
    }

    fn text(t: &str) -> ContentBlock {
        ContentBlock::Text { text: t.into() }
    }

    #[test]
    fn assistant_turns_are_relabelled_model() {
        let c = gemini_contents(&[msg("assistant", vec![text("hi")])]);
        assert_eq!(c[0]["role"], "model");
        assert_eq!(c[0]["parts"][0]["text"], "hi");
    }

    #[test]
    fn user_turns_keep_the_user_role() {
        let c = gemini_contents(&[msg("user", vec![text("hi")])]);
        assert_eq!(c[0]["role"], "user");
    }

    #[test]
    fn empty_turns_are_dropped_entirely() {
        assert!(gemini_contents(&[msg("user", vec![text("")])]).is_empty());
    }

    #[test]
    fn every_function_call_carries_a_thought_signature() {
        // Gemini 3+ answers 400 "Function call is missing a thought_signature"
        // for any functionCall part without one, so this must hold across the
        // whole replayed history, not just the newest call.
        let contents = gemini_contents(&[
            msg(
                "assistant",
                vec![
                    text("first"),
                    ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "search".into(),
                        input: serde_json::json!({}),
                    },
                ],
            ),
            msg(
                "user",
                vec![ContentBlock::ToolResult {
                    tool_use_id: "c1".into(),
                    content: "hit".into(),
                    is_error: false,
                }],
            ),
            msg(
                "assistant",
                vec![ContentBlock::ToolUse {
                    id: "c2".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "a" }),
                }],
            ),
        ]);

        let mut calls = 0;
        for content in &contents {
            for part in content["parts"].as_array().unwrap() {
                if part.get("functionCall").is_some() {
                    calls += 1;
                    let sig = part
                        .get("thoughtSignature")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    assert!(!sig.is_empty(), "unsigned functionCall: {part}");
                }
            }
        }
        assert_eq!(calls, 2, "expected both tool calls in the history");
    }

    #[test]
    fn only_function_calls_are_signed() {
        // A signature on a text or functionResponse part is not required and
        // would be noise on the wire.
        let contents = gemini_contents(&[
            msg("user", vec![text("hello")]),
            msg(
                "user",
                vec![ContentBlock::ToolResult {
                    tool_use_id: "c1".into(),
                    content: "out".into(),
                    is_error: false,
                }],
            ),
        ]);
        for content in &contents {
            for part in content["parts"].as_array().unwrap() {
                assert!(
                    part.get("thoughtSignature").is_none(),
                    "unexpected signature on {part}"
                );
            }
        }
    }

    #[test]
    fn tool_use_becomes_a_function_call_part() {
        let c = gemini_contents(&[msg(
            "assistant",
            vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "a.txt" }),
            }],
        )]);
        assert_eq!(c[0]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(c[0]["parts"][0]["functionCall"]["args"]["path"], "a.txt");
    }

    #[test]
    fn a_tool_result_turn_is_attributed_to_the_user() {
        let c = gemini_contents(&[msg(
            "assistant",
            vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "done".into(),
                is_error: false,
            }],
        )]);
        // Even though the source message said "assistant".
        assert_eq!(c[0]["role"], "user");
        assert_eq!(
            c[0]["parts"][0]["functionResponse"]["response"]["output"],
            "done"
        );
    }

    #[test]
    fn a_function_response_inherits_the_name_of_the_call_it_answers() {
        let c = gemini_contents(&[
            msg(
                "assistant",
                vec![ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                }],
            ),
            msg(
                "user",
                vec![ContentBlock::ToolResult {
                    tool_use_id: "c1".into(),
                    content: "contents".into(),
                    is_error: false,
                }],
            ),
        ]);
        assert_eq!(c[1]["parts"][0]["functionResponse"]["name"], "read_file");
    }

    #[test]
    fn several_calls_pair_with_their_responses_in_order() {
        let c = gemini_contents(&[
            msg(
                "assistant",
                vec![
                    ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "first".into(),
                        input: serde_json::json!({}),
                    },
                    ContentBlock::ToolUse {
                        id: "c2".into(),
                        name: "second".into(),
                        input: serde_json::json!({}),
                    },
                ],
            ),
            msg(
                "user",
                vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "c1".into(),
                        content: "a".into(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "c2".into(),
                        content: "b".into(),
                        is_error: false,
                    },
                ],
            ),
        ]);
        assert_eq!(c[1]["parts"][0]["functionResponse"]["name"], "first");
        assert_eq!(c[1]["parts"][1]["functionResponse"]["name"], "second");
    }

    #[test]
    fn images_become_inline_data() {
        let c = gemini_contents(&[msg(
            "user",
            vec![ContentBlock::Image {
                source: ImageSource {
                    source_type: "base64".into(),
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                },
            }],
        )]);
        assert_eq!(c[0]["parts"][0]["inlineData"]["mimeType"], "image/png");
        assert_eq!(c[0]["parts"][0]["inlineData"]["data"], "AAAA");
    }

    #[test]
    fn thinking_is_not_replayed() {
        assert!(
            gemini_contents(&[msg(
                "assistant",
                vec![ContentBlock::Thinking {
                    thinking: "hmm".into()
                }]
            )])
            .is_empty()
        );
    }

    #[test]
    fn function_names_are_coerced_into_geminis_grammar() {
        assert_eq!(sanitize_function_name("read file!"), "read_file_");
        assert_eq!(sanitize_function_name("9lives"), "_9lives");
        assert_eq!(sanitize_function_name(""), "_unknown");
        assert_eq!(sanitize_function_name("mcp__a__b"), "mcp__a__b");
        assert_eq!(sanitize_function_name("ok.name:x-y"), "ok.name:x-y");
        assert_eq!(sanitize_function_name(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn unsupported_schema_keywords_are_stripped_at_every_depth() {
        let mut schema = serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "minLength": 2, "format": "email" },
                "nested": {
                    "type": "object",
                    "properties": { "x": { "type": "number", "default": 1 } }
                }
            }
        });
        sanitize_schema(&mut schema);

        assert!(schema.get("$schema").is_none());
        assert!(schema.get("additionalProperties").is_none());
        assert!(schema["properties"]["name"].get("minLength").is_none());
        assert!(schema["properties"]["name"].get("format").is_none());
        assert!(
            schema["properties"]["nested"]["properties"]["x"]
                .get("default")
                .is_none()
        );
        // Supported keywords survive.
        assert_eq!(schema["properties"]["name"]["type"], "string");
    }

    #[test]
    fn a_node_with_properties_gains_an_object_type() {
        let mut schema = serde_json::json!({ "properties": { "a": { "type": "string" } } });
        sanitize_schema(&mut schema);
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn required_is_pruned_to_surviving_properties() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": { "kept": { "type": "string" } },
            "required": ["kept", "gone"]
        });
        sanitize_schema(&mut schema);
        assert_eq!(schema["required"], serde_json::json!(["kept"]));
    }

    #[test]
    fn a_required_list_with_nothing_left_is_removed() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {},
            "required": ["gone"]
        });
        sanitize_schema(&mut schema);
        assert!(schema.get("required").is_none());
    }

    #[test]
    fn a_union_type_collapses_to_a_single_scalar() {
        // The exact shape behind the live 400: "Proto field is not repeating,
        // cannot start list" — Gemini's `type` is a scalar, never a list.
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "maybe": { "type": ["string", "null"] },
                "multi": { "type": ["number", "string"] },
                "onlynull": { "type": ["null"] }
            }
        });
        sanitize_schema(&mut schema);

        assert_eq!(schema["properties"]["maybe"]["type"], "string");
        assert_eq!(schema["properties"]["multi"]["type"], "number");
        // Nothing usable left — fall back to a string rather than emit a list.
        assert_eq!(schema["properties"]["onlynull"]["type"], "string");
        assert_eq!(gemini_would_reject(&schema), None);
    }

    #[test]
    fn a_nullable_any_of_collapses_onto_its_real_branch() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "opt": {
                    "anyOf": [
                        { "type": "null" },
                        { "type": "object", "properties": { "x": { "type": "string" } } }
                    ]
                }
            }
        });
        sanitize_schema(&mut schema);

        let opt = &schema["properties"]["opt"];
        assert!(opt.get("anyOf").is_none());
        assert_eq!(opt["type"], "object");
        assert_eq!(opt["properties"]["x"]["type"], "string");
        assert_eq!(gemini_would_reject(&schema), None);
    }

    #[test]
    fn any_of_prefers_the_richest_branch() {
        let mut schema = serde_json::json!({
            "anyOf": [
                { "type": "string" },
                { "type": "array", "items": { "type": "string" } },
                { "type": "object", "properties": { "deep": { "type": "number" } } }
            ]
        });
        sanitize_schema(&mut schema);
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["deep"]["type"], "number");
    }

    #[test]
    fn one_of_is_collapsed_like_any_of() {
        let mut schema = serde_json::json!({
            "oneOf": [{ "type": "null" }, { "type": "integer" }]
        });
        sanitize_schema(&mut schema);
        assert_eq!(schema["type"], "integer");
        assert!(schema.get("oneOf").is_none());
    }

    #[test]
    fn all_of_branches_merge_into_the_parent() {
        let mut schema = serde_json::json!({
            "allOf": [
                { "properties": { "a": { "type": "string" } }, "required": ["a"] },
                { "properties": { "b": { "type": "number" } }, "required": ["b"] }
            ]
        });
        sanitize_schema(&mut schema);

        assert!(schema.get("allOf").is_none());
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["a"]["type"], "string");
        assert_eq!(schema["properties"]["b"]["type"], "number");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("a")));
        assert!(required.contains(&serde_json::json!("b")));
    }

    #[test]
    fn const_becomes_a_single_valued_string_enum() {
        let mut schema = serde_json::json!({ "const": "fixed" });
        sanitize_schema(&mut schema);
        assert!(schema.get("const").is_none());
        assert_eq!(schema["enum"], serde_json::json!(["fixed"]));
        // Gemini requires the type alongside an enum.
        assert_eq!(schema["type"], "string");
    }

    #[test]
    fn enum_values_are_stringified_and_typed() {
        let mut schema = serde_json::json!({ "enum": [1, 2, true] });
        sanitize_schema(&mut schema);
        assert_eq!(schema["enum"], serde_json::json!(["1", "2", "true"]));
        assert_eq!(schema["type"], "string");
    }

    #[test]
    fn a_realistic_mcp_tool_schema_survives_intact() {
        // Composite of the constructs that appeared in the failing payload.
        let mut schema = serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path":    { "type": "string", "minLength": 1, "format": "uri" },
                "limit":   { "type": ["integer", "null"], "default": 10 },
                "mode":    { "const": "fast" },
                "filter":  { "anyOf": [{ "type": "null" }, { "type": "string" }] },
                "opts":    {
                    "allOf": [
                        { "properties": { "deep": { "type": ["boolean", "null"] } } }
                    ]
                },
                "items":   { "type": "array", "items": { "type": ["string", "null"] } }
            },
            "required": ["path", "gone"]
        });
        sanitize_schema(&mut schema);

        assert_eq!(gemini_would_reject(&schema), None, "{schema:#}");
        // The useful shape is preserved, not just stripped.
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["limit"]["type"], "integer");
        assert_eq!(schema["properties"]["filter"]["type"], "string");
        assert_eq!(schema["properties"]["opts"]["properties"]["deep"]["type"], "boolean");
        assert_eq!(schema["properties"]["items"]["items"]["type"], "string");
        // `gone` names no property and must not survive.
        assert_eq!(schema["required"], serde_json::json!(["path"]));
    }

    #[test]
    fn every_tool_declaration_is_accepted_after_sanitising() {
        // Guards the whole pipeline: nothing gemini_would_reject may remain in
        // any declaration the adapter emits.
        let mut nasty = serde_json::json!({
            "type": ["object", "null"],
            "properties": {
                "a": { "anyOf": [{ "type": "null" }, { "type": ["string", "null"] }] }
            },
            "required": ["a"]
        });
        sanitize_schema(&mut nasty);
        assert_eq!(gemini_would_reject(&nasty), None, "{nasty:#}");
        assert_eq!(nasty["type"], "object");
        assert_eq!(nasty["properties"]["a"]["type"], "string");
    }

    #[test]
    fn required_is_dropped_when_there_are_no_properties_to_match() {
        let mut schema = serde_json::json!({ "type": "object", "required": ["x"] });
        sanitize_schema(&mut schema);
        assert!(schema.get("required").is_none());
    }

    #[test]
    fn sanitising_is_idempotent() {
        let mut once = serde_json::json!({
            "type": "object",
            "additionalProperties": true,
            "properties": { "a": { "type": "string", "format": "uri" } },
            "required": ["a"]
        });
        sanitize_schema(&mut once);
        let mut twice = once.clone();
        sanitize_schema(&mut twice);
        assert_eq!(once, twice);
    }

    fn feed(acc: &mut GeminiAccumulator, events: &[Value]) {
        for e in events {
            acc.apply(e);
        }
    }

    #[test]
    fn text_parts_accumulate_through_the_response_envelope() {
        let mut acc = GeminiAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"response":{"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}}),
                serde_json::json!({"response":{"candidates":[{"content":{"parts":[{"text":"lo"}]}}]}}),
            ],
        );
        assert_eq!(acc.text, "Hello");
    }

    #[test]
    fn a_bare_gemini_chunk_without_the_envelope_also_parses() {
        let mut acc = GeminiAccumulator::default();
        feed(
            &mut acc,
            &[serde_json::json!({"candidates":[{"content":{"parts":[{"text":"hi"}]}}]})],
        );
        assert_eq!(acc.text, "hi");
    }

    #[test]
    fn thought_parts_are_kept_apart_from_output() {
        let mut acc = GeminiAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"response":{"candidates":[{"content":{"parts":[
                    {"text":"planning","thought":true},
                    {"text":"answer"}
                ]}}]}}),
            ],
        );
        assert_eq!(acc.reasoning, "planning");
        assert_eq!(acc.text, "answer");
    }

    #[test]
    fn function_calls_are_collected_with_synthetic_ids() {
        let mut acc = GeminiAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"response":{"candidates":[{"content":{"parts":[
                    {"functionCall":{"name":"read","args":{"path":"a"}}},
                    {"functionCall":{"name":"write","args":{}}}
                ]}}]}}),
            ],
        );
        let calls = acc.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["name"], "read");
        assert_eq!(calls[0]["input"]["path"], "a");
        assert_ne!(calls[0]["id"], calls[1]["id"], "ids must be distinct");
    }

    #[test]
    fn usage_metadata_maps_onto_raw_usage() {
        let mut acc = GeminiAccumulator::default();
        feed(
            &mut acc,
            &[serde_json::json!({"response":{
                "candidates":[{"content":{"parts":[]}}],
                "usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":7}
            }})],
        );
        let usage = acc.usage.expect("usage recorded");
        assert_eq!(usage.input_tokens, Some(11));
        assert_eq!(usage.output_tokens, Some(7));
    }

    #[test]
    fn a_completed_stream_builds_the_assistant_message() {
        let mut acc = GeminiAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"response":{"candidates":[{"content":{"parts":[
                    {"text":"thinking","thought":true},
                    {"text":"hello"},
                    {"functionCall":{"name":"t","args":{}}}
                ]}}]}}),
            ],
        );
        let msg = acc.into_message().unwrap();
        assert_eq!(msg.message.role, "assistant");
        assert_eq!(msg.message.content.len(), 3);
        assert!(matches!(
            msg.message.content[0],
            ContentBlock::Thinking { .. }
        ));
        assert!(matches!(msg.message.content[1], ContentBlock::Text { .. }));
        assert!(matches!(
            msg.message.content[2],
            ContentBlock::ToolUse { .. }
        ));
    }

    #[test]
    fn malformed_chunks_do_not_panic() {
        let mut acc = GeminiAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({}),
                serde_json::json!({"response":{}}),
                serde_json::json!({"response":{"candidates":[]}}),
                serde_json::json!({"response":{"candidates":[{"content":{}}]}}),
            ],
        );
        assert!(acc.text.is_empty());
        assert!(acc.tool_calls().is_empty());
    }

    #[test]
    fn sse_lines_yield_only_real_payloads() {
        assert!(parse_sse_line("data: {\"a\":1}").is_some());
        assert!(parse_sse_line("data: [DONE]").is_none());
        assert!(parse_sse_line("event: x").is_none());
        assert!(parse_sse_line("").is_none());
    }

    #[test]
    fn stable_ids_are_deterministic_and_uuid_shaped() {
        let a = stable_id("seed");
        assert_eq!(a, stable_id("seed"));
        assert_ne!(a, stable_id("other"));
        let groups: Vec<&str> = a.split('-').collect();
        assert_eq!(groups.len(), 5);
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
    }

    #[test]
    fn request_ids_follow_the_ide_shape() {
        let id = build_request_id("sess", "gemini-3", 3);
        let parts: Vec<&str> = id.split('/').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0], "agent");
        // Same inputs must produce the same lineage.
        assert_eq!(id, build_request_id("sess", "gemini-3", 3));
    }

    #[test]
    fn a_project_is_read_from_either_reply_shape() {
        // Google returns a bare string...
        assert_eq!(
            extract_project(Some(&serde_json::json!("projects/42"))).as_deref(),
            Some("projects/42")
        );
        // ...or an object carrying the id.
        assert_eq!(
            extract_project(Some(&serde_json::json!({ "id": "projects/7" }))).as_deref(),
            Some("projects/7")
        );
        // Surrounding whitespace is trimmed.
        assert_eq!(
            extract_project(Some(&serde_json::json!("  p  "))).as_deref(),
            Some("p")
        );
    }

    #[test]
    fn an_absent_or_blank_project_reads_as_none() {
        assert_eq!(extract_project(None), None);
        assert_eq!(extract_project(Some(&Value::Null)), None);
        assert_eq!(extract_project(Some(&serde_json::json!(""))), None);
        assert_eq!(extract_project(Some(&serde_json::json!("   "))), None);
        assert_eq!(extract_project(Some(&serde_json::json!({}))), None);
        assert_eq!(extract_project(Some(&serde_json::json!(42))), None);
    }

    #[test]
    fn the_default_tier_is_picked_out_of_allowed_tiers() {
        let reply = serde_json::json!({
            "allowedTiers": [
                { "id": "free-tier", "isDefault": false },
                { "id": "standard-tier", "isDefault": true }
            ]
        });
        assert_eq!(extract_default_tier(&reply), "standard-tier");
    }

    #[test]
    fn tier_extraction_falls_back_when_none_is_marked_default() {
        assert_eq!(extract_default_tier(&serde_json::json!({})), "legacy-tier");
        assert_eq!(
            extract_default_tier(&serde_json::json!({ "allowedTiers": [] })),
            "legacy-tier"
        );
        assert_eq!(
            extract_default_tier(
                &serde_json::json!({ "allowedTiers": [{ "id": "x", "isDefault": false }] })
            ),
            "legacy-tier"
        );
        // A default entry with a blank id is not usable.
        assert_eq!(
            extract_default_tier(
                &serde_json::json!({ "allowedTiers": [{ "id": "  ", "isDefault": true }] })
            ),
            "legacy-tier"
        );
    }

    #[test]
    fn a_rejected_project_is_recognised_so_the_cache_can_be_busted() {
        // The live 403 that a generated project id produced.
        let body = r#"{"error":{"code":403,"message":"Permission denied on resource project example-proj-0a1b2c.","status":"PERMISSION_DENIED","details":[{"reason":"CONSUMER_INVALID"}]}}"#;
        assert!(is_project_rejected(403, body));
    }

    #[test]
    fn other_failures_do_not_bust_the_project_cache() {
        // A 401 is an auth problem, not a project problem — the token refresh
        // path handles it, and clearing the project would be wrong.
        assert!(!is_project_rejected(401, "invalid token"));
        assert!(!is_project_rejected(429, "rate limited"));
        assert!(!is_project_rejected(500, "PERMISSION_DENIED"));
        // A 403 for an unrelated reason leaves the project alone.
        assert!(!is_project_rejected(403, r#"{"error":"quota exhausted"}"#));
    }

    #[test]
    fn synthetic_project_labels_are_stable_and_well_shaped() {
        let a = synthetic_project_id("acct-1");
        assert_eq!(a, synthetic_project_id("acct-1"), "must be deterministic");
        assert_ne!(a, synthetic_project_id("acct-2"));
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 3, "{a}");
        assert_eq!(parts[2].len(), 6, "{a}");
    }

    #[test]
    fn an_onboarding_project_is_read_from_either_reply_shape() {
        // Long-running-operation shape.
        let nested = serde_json::json!({
            "done": true,
            "response": { "cloudaicompanionProject": "projects/7" }
        });
        assert_eq!(
            extract_project(nested.get("response").and_then(|r| r.get("cloudaicompanionProject")))
                .as_deref(),
            Some("projects/7")
        );

        // Flat shape — the fallback the poll loop also checks.
        let flat = serde_json::json!({ "done": true, "cloudaicompanionProject": "projects/9" });
        assert_eq!(
            extract_project(flat.get("cloudaicompanionProject")).as_deref(),
            Some("projects/9")
        );
    }

    #[test]
    fn json_truncation_is_char_boundary_safe() {
        let value = serde_json::json!({ "msg": "kèm dấu tiếng Việt ".repeat(50) });
        let out = truncate_json(&value, 20);
        assert_eq!(out.chars().count(), 21); // 20 + ellipsis
    }

    #[test]
    fn discovery_carries_the_headers_code_assist_requires() {
        let headers = discovery_headers("tok");
        let get = |k: &str| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("Authorization"), Some("Bearer tok"));
        assert_eq!(get("Content-Type"), Some("application/json"));
        // Without this the endpoint rejects the call outright.
        let meta = get("Client-Metadata").expect("client metadata header");
        assert!(meta.contains("ideType"), "{meta}");

        // Discovery carries the API-client library identity, which differs from
        // the IDE identity the completion call sends.
        assert_eq!(
            get("User-Agent"),
            Some(crate::providers::oauth::transport::CODE_ASSIST_DISCOVERY_USER_AGENT)
        );
        assert_eq!(
            get("X-Goog-Api-Client"),
            Some(crate::providers::oauth::transport::CODE_ASSIST_DISCOVERY_API_CLIENT)
        );
    }

    #[test]
    fn tool_declarations_dedupe_and_sanitise() {
        // Two tools whose names collide after sanitising must not both appear.
        assert_eq!(sanitize_function_name("a b"), sanitize_function_name("a_b"));
    }
}

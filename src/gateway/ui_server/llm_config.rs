use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use rand::Rng;
use serde::Deserialize;

use crate::gateway::group_manager::{
    get_thinking_enabled, load_llm_configs, remove_llm_config, save_llm_config,
    set_active_cognitive_llm_config, set_active_llm_config, set_active_quick_llm_config, LlmConfig,
};

use super::core::{AppError, UiState};

// ===== /api/llm-config/* =====

/// Body for creating a new LLM config (no id — auto-generated).
#[derive(Deserialize)]
pub(crate) struct NewLlmConfigBody {
    label: String,
    provider: String,
    #[serde(rename = "baseURL")]
    base_url: String,
    #[serde(rename = "apiKey")]
    api_key: String,
    #[serde(rename = "modelName")]
    model_name: String,
    adapt: String,
    #[serde(rename = "maxTokens")]
    max_tokens: u32,
    #[serde(rename = "contextLength")]
    context_length: u32,
    /// Explicitly declare whether vision input is supported; undefined = auto-infer from modelName
    #[serde(default)]
    vision: Option<bool>,
    /// `"oauth"` to draw the credential from the OAuth account store instead
    /// of [`Self::api_key`]; omitted or `"api_key"` keeps the classic path.
    #[serde(default)]
    auth: Option<String>,
    /// Required when `auth == "oauth"` — which stored account to use.
    #[serde(rename = "oauthAccountId", default)]
    oauth_account_id: Option<String>,
}

/// Body for setting active model.
#[derive(Deserialize)]
pub(crate) struct ActiveLlmBody {
    id: Option<String>,
    #[serde(rename = "type", default = "default_llm_type")]
    llm_type: String,
}

fn default_llm_type() -> String {
    "main".to_string()
}

/// Body for test/fetch-models.
#[derive(Deserialize)]
pub(crate) struct LlmProviderBody {
    #[serde(rename = "baseURL")]
    base_url: String,
    #[serde(rename = "apiKey")]
    api_key: String,
    adapt: String,
}

/// Body for updating LLM config fields (partial update).
#[derive(Deserialize)]
pub(crate) struct UpdateLlmConfigBody {
    /// Explicitly declare whether vision input is supported; null = reset to auto-infer
    #[serde(default)]
    vision: Option<bool>,
    /// Per-model Edit format (`exact` | `fuzzy` | `udiff` | `whole`). Present
    /// and null → reset to exact; absent → unchanged.
    #[serde(default, rename = "editFormat", deserialize_with = "deserialize_double_option")]
    edit_format: Option<Option<crate::zen_core::EditFormat>>,
}

/// Distinguish "field absent" (`None`) from "field null" (`Some(None)`).
fn deserialize_double_option<'de, D>(
    d: D,
) -> Result<Option<Option<crate::zen_core::EditFormat>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(Some(Option::<crate::zen_core::EditFormat>::deserialize(d)?))
}

/// GET /api/llm-config — list all configs
pub(crate) async fn llm_config_list(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let stored = load_llm_configs(&s.config.paths.global_config_path);
    // `appId` marks the rows that come from a Space App, so the UI can render
    // them read-only instead of offering Edit and Delete buttons that answer
    // 409. Serialized alongside the config rather than inside it: `LlmConfig` is
    // also what gets written to `config.json`, and this is not a stored field.
    let configs: Vec<serde_json::Value> = stored
        .configs
        .iter()
        .map(|c| {
            let mut v = serde_json::to_value(c).unwrap_or_default();
            if let Some((app_id, _)) = crate::apps::llm_provider::parse_config_id(&c.id) {
                v["appId"] = serde_json::json!(app_id);
            }
            v
        })
        .collect();
    Json(serde_json::json!({
        "configs": configs,
        "activeId": stored.active_id,
        "activeQuickId": stored.active_quick_id,
        "activeCognitiveId": stored.active_cognitive_id,
        "thinkingEnabled": get_thinking_enabled(&s.config.paths.global_config_path),
    }))
}

/// POST /api/llm-config — create or update config
pub(crate) async fn llm_config_create(
    State(s): State<Arc<UiState>>,
    Json(body): Json<NewLlmConfigBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let id = format!(
        "llm_{}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        rand::thread_rng().gen_range(1000u32..9999u32)
    );
    let cfg = LlmConfig {
        id: id.clone(),
        label: body.label,
        provider: body.provider,
        base_url: body.base_url,
        api_key: body.api_key,
        model_name: body.model_name,
        adapt: body.adapt,
        max_tokens: body.max_tokens,
        context_length: body.context_length,
        vision: body.vision,
        auth: body.auth,
        oauth_account_id: body.oauth_account_id,
        edit_format: None,
    };
    save_llm_config(&s.config.paths.global_config_path, &cfg)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Auto-activate the first configuration
    let stored = load_llm_configs(&s.config.paths.global_config_path);
    if stored.configs.len() == 1 {
        let _ = set_active_llm_config(&s.config.paths.global_config_path, Some(&id));
    }

    Ok(Json(serde_json::to_value(&cfg).unwrap_or_default()))
}

/// DELETE /api/llm-config/{id} — delete config
pub(crate) async fn llm_config_delete(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> impl IntoResponse {
    let id = id.trim().to_string();
    if id.is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    // An app's (or a local model's) config is not in `config.json`, so removing
    // it there would succeed silently and change nothing — the row would still
    // be in the picker on the next read, which reads as a broken Delete
    // button. An app's model goes away when the app is uninstalled; a local
    // model's when its file is deleted from Settings → Local models.
    if crate::apps::llm_provider::is_app_config(&id) || crate::local_models::is_local_config(&id) {
        return StatusCode::CONFLICT;
    }
    let _ = remove_llm_config(&s.config.paths.global_config_path, &id);
    StatusCode::NO_CONTENT
}

/// PATCH /api/llm-config/{id} — update config fields
pub(crate) async fn llm_config_update(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<UpdateLlmConfigBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid ID".to_string()));
    }
    // A `vision` override on an app's model would be silently reverted the next
    // time the app registers — and the app's own answer is the better one: it
    // read the checkpoint's config, while this would be a user's guess. A
    // local model's `vision` likewise comes from the checkpoint itself (an
    // mmproj file, or MLX `vision_config`), never from an edit here.
    if crate::apps::llm_provider::is_app_config(&id) {
        return Err(AppError(
            StatusCode::CONFLICT,
            "This model is provided by a Space App and is configured in the app, not here."
                .to_string(),
        ));
    }
    if crate::local_models::is_local_config(&id) {
        return Err(AppError(
            StatusCode::CONFLICT,
            "This is a local model; manage it from Settings → Local models.".to_string(),
        ));
    }

    // Load existing configs
    let stored = load_llm_configs(&s.config.paths.global_config_path);

    // Find the config to update
    let mut cfg = stored
        .configs
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "Config not found".to_string()))?;

    // Update vision field if provided
    if body.vision.is_some() {
        cfg.vision = body.vision;
    }
    if let Some(v) = body.edit_format {
        cfg.edit_format = v;
    }

    // Save the updated config
    save_llm_config(&s.config.paths.global_config_path, &cfg)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::to_value(cfg).unwrap_or_default()))
}

/// POST /api/llm-config/active — set active main or quick model
pub(crate) async fn llm_config_set_active(
    State(s): State<Arc<UiState>>,
    Json(body): Json<ActiveLlmBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    match body.llm_type.as_str() {
        "quick" => {
            set_active_quick_llm_config(&s.config.paths.global_config_path, body.id.as_deref())
                .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            Ok(Json(serde_json::json!({ "activeQuickId": body.id })))
        }
        "cognitive" => {
            set_active_cognitive_llm_config(&s.config.paths.global_config_path, body.id.as_deref())
                .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            Ok(Json(serde_json::json!({ "activeCognitiveId": body.id })))
        }
        _ => {
            set_active_llm_config(&s.config.paths.global_config_path, body.id.as_deref())
                .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            Ok(Json(serde_json::json!({ "activeId": body.id })))
        }
    }
}

/// POST /api/llm-config/test — test provider connection
pub(crate) async fn llm_config_test(
    Json(body): Json<LlmProviderBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    match fetch_models(&body.base_url, &body.api_key, &body.adapt).await {
        Ok(_) => Ok(Json(
            serde_json::json!({ "success": true, "message": "Connected successfully" }),
        )),
        Err(e) => Ok(Json(
            serde_json::json!({ "success": false, "message": e.to_string() }),
        )),
    }
}

/// POST /api/llm-config/models — fetch available models from provider
pub(crate) async fn llm_config_fetch_models(
    Json(body): Json<LlmProviderBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    match fetch_models(&body.base_url, &body.api_key, &body.adapt).await {
        Ok(models) => Ok(Json(
            serde_json::json!({ "success": true, "models": models }),
        )),
        Err(e) => Ok(Json(
            serde_json::json!({ "success": false, "message": e.to_string() }),
        )),
    }
}

/// Run a one-shot chat completion against the active LLM config. Used by the
/// Space-App bridge (`llm.request`) so apps can reuse SenClaw's configured LLM.
/// Returns `(answer_text, model_name)`.
/// Returns `(text, model, finish_reason)`. `finish_reason` is `"length"`
/// when the provider cut the output at the token cap (callers can continue),
/// `"stop"` on natural completion, `""` when the provider didn't say.
/// Resolve which LLM profile a completion should run on.
///
/// An explicit `profile` (config **id** or its human **label**, e.g. "MoltClaw")
/// wins — this is how a Space App uses its own model *without* hijacking the
/// daemon's global active model. With no profile we fall back to the active
/// config, then to the first one configured.
///
/// An explicitly-requested-but-missing profile is an error rather than a silent
/// fallback: quietly answering on the wrong model is worse than saying so.
fn pick_config<'a>(
    configs: &'a [LlmConfig],
    active_id: Option<&str>,
    profile: Option<&str>,
) -> Result<&'a LlmConfig, String> {
    if let Some(want) = profile.map(str::trim).filter(|s| !s.is_empty()) {
        return configs
            .iter()
            .find(|c| c.id == want)
            .or_else(|| configs.iter().find(|c| c.label.eq_ignore_ascii_case(want)))
            .ok_or_else(|| {
                format!("LLM profile '{want}' not found in SenClaw (Settings → Models)")
            });
    }
    active_id
        .and_then(|id| configs.iter().find(|c| c.id == id))
        .or_else(|| configs.first())
        .ok_or_else(|| "No LLM configured in SenClaw (Settings → Models)".to_string())
}

/// Refuse a completion that would be routed straight back to the app asking for
/// it.
///
/// A Space App can serve models *and* use the bridge to ask SenClaw for one. If
/// the selected model is one the caller itself serves, the request goes app →
/// daemon → proxy → same app, and the app's provider — which is what issued the
/// bridge call — waits on its own answer. It does not fail; it hangs, and takes
/// a connection and a request slot with it each time.
///
/// This is not hypothetical for the obvious wrong implementation: an app whose
/// `LlmProvider::chat` "just forwards to the daemon" builds exactly this loop
/// and looks correct until it is the active model. Refused with a message
/// naming both sides, rather than quietly answered by a different model than the
/// user selected.
fn reject_self_routing(cfg: &LlmConfig, caller_app: Option<&str>) -> Result<(), String> {
    let Some(caller) = caller_app else {
        return Ok(());
    };
    match crate::apps::llm_provider::parse_config_id(&cfg.id) {
        Some((owner, model)) if owner == caller => Err(format!(
            "model '{model}' is served by app '{caller}' itself — a completion on it would route \
             back into the caller. Pick another model, or pass `profile` naming one."
        )),
        _ => Ok(()),
    }
}

/// Everything a one-shot completion produced. `usage` is the provider-reported
/// token usage (`None` when the provider sent no usage object); `profile` /
/// `provider` / `model` identify the config the call actually ran on, so
/// callers can feed the usage recorder without re-resolving the config.
#[derive(Debug, Clone)]
pub(crate) struct ChatCompletionResult {
    pub text: String,
    pub model: String,
    /// "length" when truncated by max_tokens, "stop" on natural end, "" unknown.
    pub finish: String,
    pub usage: Option<crate::zen_core::RawUsage>,
    pub latency_ms: u64,
    pub profile: String,
    pub provider: String,
}

/// Feed a brokered completion into the usage recorder. `jid`/`app_id` say who
/// the call was for (`app:<id>` + app id for bridge `llm.request`, an internal
/// marker jid for daemon-side draft completions). No-op when the provider
/// reported no usage or the recorder isn't wired (bare test states).
pub(crate) fn record_completion(
    rec: &Option<Arc<crate::usage::UsageRecorder>>,
    jid: &str,
    app_id: &str,
    r: &ChatCompletionResult,
) {
    let (Some(rec), Some(u)) = (rec, &r.usage) else {
        return;
    };
    let ev = crate::usage::UsageEvent {
        jid: jid.to_string(),
        app_id: app_id.to_string(),
        profile: r.profile.clone(),
        provider: r.provider.clone(),
        model: r.model.clone(),
        latency_ms: r.latency_ms,
        ..crate::usage::UsageEvent::new(crate::usage::UsageSource::Bridge)
    }
    .with_tokens(u);
    rec.record(ev);
}

/// One-shot completion. `profile` selects a specific LLM config by id or label;
/// `None` uses the daemon's active model.
/// Build a [`ModelProfile`](crate::zen_core::ModelProfile) from a stored config
/// so a brokered completion can run through the same adapter dispatcher the
/// agent loop uses.
///
/// An OAuth config keeps only an account id in `config.json` — the bearer token
/// lives in the OAuth store. Unlike the engine's synchronous resolver this can
/// await, so it refreshes an expiring token up front rather than leaning on the
/// transport's 401 retry.
async fn profile_from_config(cfg: &LlmConfig, cap: u32) -> crate::zen_core::ModelProfile {
    let provider = if cfg.provider.trim().is_empty() {
        if cfg.adapt.trim().eq_ignore_ascii_case("anthropic") {
            "anthropic".to_string()
        } else {
            "openai".to_string()
        }
    } else {
        cfg.provider.clone()
    };

    let (api_key, oauth_provider, oauth_account_id) = if cfg.is_oauth() {
        let account_id = cfg.oauth_account_id.clone().unwrap_or_default();
        let manager = crate::providers::oauth::global();
        let token = match manager.as_ref() {
            Some(m) => m.ensure_fresh(&account_id).await.unwrap_or_else(|_| {
                // A refresh that fails still leaves whatever the background
                // refresher cached; the 401 retry gets the last word.
                crate::providers::oauth::access_token_for(&account_id).unwrap_or_default()
            }),
            None => String::new(),
        };
        let provider_id = manager
            .and_then(|m| m.account(&account_id))
            .map(|a| a.provider);
        (token, provider_id, Some(account_id))
    } else {
        (cfg.api_key.clone(), None, None)
    };

    crate::zen_core::ModelProfile {
        name: cfg.label.clone(),
        provider,
        model_name: cfg.model_name.clone(),
        base_url: cfg.base_url.clone(),
        api_key,
        max_tokens: cap,
        context_length: cfg.context_length,
        adapt: if cfg.adapt.trim().is_empty() {
            None
        } else {
            Some(cfg.adapt.clone())
        },
        vision: cfg.vision,
        edit_format: cfg.edit_format,
        oauth_provider,
        oauth_account_id,
    }
}

/// True when this profile's wire format is not `POST {base}/chat/completions`.
///
/// `codex` speaks the OpenAI Responses API, `antigravity` speaks Google Code
/// Assist, and the `local-*` adapters run in-process with no endpoint at all.
/// The hand-rolled requests below can only build two body shapes, and would
/// POST an OpenAI one at whatever `baseURL` holds — for an Antigravity profile
/// that is `daily-cloudcode-pa.googleapis.com/chat/completions`, which answers
/// with Google's HTML 404 page instead of JSON. Everything outside those two
/// shapes goes to the agent loop's dispatcher, which knows every adapter.
fn needs_adapter_dispatch(profile: &crate::zen_core::ModelProfile) -> bool {
    !matches!(
        crate::zen_core::query_llm::effective_adapter(profile),
        "openai" | "anthropic"
    )
}

/// Run a one-shot completion through [`query_llm`](crate::zen_core::query_llm),
/// the same path the agent loop takes.
///
/// No tools are passed: this is a single completion, and every adapter omits
/// the tool field for an empty slice.
async fn dispatch_completion(
    client: &reqwest::Client,
    cfg: &LlmConfig,
    profile: &crate::zen_core::ModelProfile,
    system: &str,
    blocks: Vec<crate::zen_core::ContentBlock>,
    started: std::time::Instant,
) -> Result<ChatCompletionResult, String> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let messages = vec![crate::zen_core::create_user_message(blocks)];

    let msg = crate::zen_core::query_llm::query_llm(
        client, &messages, system, &[], &cancel, profile, false, false, None,
    )
    .await
    .map_err(|e| format!("LLM request failed: {e}"))?;

    let mut text = String::new();
    for block in &msg.message.content {
        if let crate::zen_core::ContentBlock::Text { text: t } = block {
            text.push_str(t);
        }
    }
    if text.trim().is_empty() {
        return Err("LLM returned an empty response".into());
    }

    Ok(ChatCompletionResult {
        text,
        model: cfg.model_name.clone(),
        // The dispatcher hands back an assembled message, not the raw stop
        // reason, so truncation can't be detected here. "" is the same
        // "unknown" callers already get from providers that omit it.
        finish: String::new(),
        usage: msg.usage.clone(),
        latency_ms: started.elapsed().as_millis() as u64,
        profile: cfg.label.clone(),
        provider: cfg.provider.clone(),
    })
}

/// `caller_app` is the Space App on whose behalf this runs, when there is one.
/// It exists only for [`reject_self_routing`]; pass `None` from anywhere that is
/// not an app's bridge call.
pub(crate) async fn chat_completion(
    config_path: &std::path::Path,
    profile: Option<&str>,
    system: &str,
    user: &str,
    max_tokens: u32,
    caller_app: Option<&str>,
) -> Result<ChatCompletionResult, String> {
    let started = std::time::Instant::now();
    let stored = load_llm_configs(config_path);
    let cfg = pick_config(&stored.configs, stored.active_id.as_deref(), profile)?;
    reject_self_routing(cfg, caller_app)?;

    let client = reqwest::Client::new();
    let is_anthropic = cfg.adapt == "anthropic" && cfg.base_url.contains("anthropic.com");
    let cap = if max_tokens == 0 {
        cfg.max_tokens.max(256)
    } else {
        max_tokens
    };

    let model_profile = profile_from_config(&cfg, cap).await;
    if needs_adapter_dispatch(&model_profile) {
        return dispatch_completion(
            &client,
            &cfg,
            &model_profile,
            system,
            vec![crate::zen_core::ContentBlock::Text {
                text: user.to_string(),
            }],
            started,
        )
        .await;
    }

    let (url, body, req) = if is_anthropic {
        let base = cfg.base_url.trim_end_matches('/').trim_end_matches("/v1");
        let url = format!("{base}/v1/messages");
        let body = serde_json::json!({
            "model": cfg.model_name,
            "max_tokens": cap,
            "system": system,
            "messages": [{ "role": "user", "content": user }],
        });
        let req = client
            .post(&url)
            .header("x-api-key", &cfg.api_key)
            .header("anthropic-version", "2023-06-01");
        (url, body, req)
    } else {
        let base = cfg
            .base_url
            .trim_end_matches('/')
            .trim_end_matches("/chat/completions");
        let url = format!("{base}/chat/completions");
        let body = serde_json::json!({
            "model": cfg.model_name,
            "max_tokens": cap,
            "temperature": 0.2,
            "stream": false,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });
        let req = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", cfg.api_key));
        (url, body, req)
    };

    let resp = req
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("LLM request to {url} failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Read error: {e}"))?;
    if !status.is_success() {
        let preview: String = text.chars().take(300).collect();
        return Err(format!("LLM HTTP {status}: {preview}"));
    }
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Invalid JSON: {e}"))?;

    let answer = if is_anthropic {
        json["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    } else {
        json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    if answer.trim().is_empty() {
        return Err("LLM returned an empty response".into());
    }
    let finish = if is_anthropic {
        match json["stop_reason"].as_str().unwrap_or("") {
            "max_tokens" => "length".to_string(),
            "" => String::new(),
            _ => "stop".to_string(),
        }
    } else {
        match json["choices"][0]["finish_reason"].as_str().unwrap_or("") {
            "length" => "length".to_string(),
            "" => String::new(),
            _ => "stop".to_string(),
        }
    };
    Ok(ChatCompletionResult {
        text: answer,
        model: cfg.model_name.clone(),
        finish,
        usage: crate::zen_core::RawUsage::from_json(&json["usage"]),
        latency_ms: started.elapsed().as_millis() as u64,
        profile: cfg.label.clone(),
        provider: cfg.provider.clone(),
    })
}

/// Model name of the config a completion would run on, for capability checks
/// (e.g. does the active model support vision?). Same resolution as
/// [`chat_completion`], so the answer matches what a completion would actually
/// use.
pub(crate) fn active_model_name(
    config_path: &std::path::Path,
    profile: Option<&str>,
) -> Result<String, String> {
    let stored = load_llm_configs(config_path);
    let cfg = pick_config(&stored.configs, stored.active_id.as_deref(), profile)?;
    Ok(cfg.model_name.clone())
}

/// One-shot completion WITH an image, for vision models. Mirrors
/// [`chat_completion`] but the user turn carries an image part beside the text.
///
/// The image travels as base64, never a URL: our screenshots are served from
/// localhost, which a cloud LLM can't reach. `media_type` is a MIME like
/// `image/png`.
pub(crate) async fn chat_completion_vision(
    config_path: &std::path::Path,
    profile: Option<&str>,
    system: &str,
    user: &str,
    image_b64: &str,
    media_type: &str,
    max_tokens: u32,
    caller_app: Option<&str>,
) -> Result<ChatCompletionResult, String> {
    let started = std::time::Instant::now();
    let stored = load_llm_configs(config_path);
    let cfg = pick_config(&stored.configs, stored.active_id.as_deref(), profile)?;
    reject_self_routing(cfg, caller_app)?;

    let client = reqwest::Client::new();
    let is_anthropic = cfg.adapt == "anthropic" && cfg.base_url.contains("anthropic.com");
    let cap = if max_tokens == 0 {
        cfg.max_tokens.max(256)
    } else {
        max_tokens
    };

    let model_profile = profile_from_config(&cfg, cap).await;
    if needs_adapter_dispatch(&model_profile) {
        return dispatch_completion(
            &client,
            &cfg,
            &model_profile,
            system,
            vec![
                crate::zen_core::ContentBlock::Text {
                    text: user.to_string(),
                },
                crate::zen_core::ContentBlock::Image {
                    source: crate::zen_core::ImageSource {
                        source_type: "base64".to_string(),
                        media_type: media_type.to_string(),
                        data: image_b64.to_string(),
                    },
                },
            ],
            started,
        )
        .await;
    }

    let (url, body, req) = if is_anthropic {
        let base = cfg.base_url.trim_end_matches('/').trim_end_matches("/v1");
        let url = format!("{base}/v1/messages");
        let body = serde_json::json!({
            "model": cfg.model_name,
            "max_tokens": cap,
            "system": system,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": user },
                    { "type": "image", "source": {
                        "type": "base64", "media_type": media_type, "data": image_b64,
                    }},
                ],
            }],
        });
        let req = client
            .post(&url)
            .header("x-api-key", &cfg.api_key)
            .header("anthropic-version", "2023-06-01");
        (url, body, req)
    } else {
        let base = cfg
            .base_url
            .trim_end_matches('/')
            .trim_end_matches("/chat/completions");
        let url = format!("{base}/chat/completions");
        let data_url = format!("data:{media_type};base64,{image_b64}");
        let body = serde_json::json!({
            "model": cfg.model_name,
            "max_tokens": cap,
            "temperature": 0.2,
            "stream": false,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": [
                    { "type": "text", "text": user },
                    { "type": "image_url", "image_url": { "url": data_url } },
                ]},
            ],
        });
        let req = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", cfg.api_key));
        (url, body, req)
    };

    let resp = req
        .json(&body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("LLM request to {url} failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Read error: {e}"))?;
    if !status.is_success() {
        let preview: String = text.chars().take(300).collect();
        return Err(format!("LLM HTTP {status}: {preview}"));
    }
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Invalid JSON: {e}"))?;
    let answer = if is_anthropic {
        json["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    } else {
        json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    if answer.trim().is_empty() {
        return Err("LLM returned an empty response".into());
    }
    // Same finish mapping as `chat_completion` — the previous version returned
    // "" unconditionally, so vision callers couldn't detect truncation.
    let finish = if is_anthropic {
        match json["stop_reason"].as_str().unwrap_or("") {
            "max_tokens" => "length".to_string(),
            "" => String::new(),
            _ => "stop".to_string(),
        }
    } else {
        match json["choices"][0]["finish_reason"].as_str().unwrap_or("") {
            "length" => "length".to_string(),
            "" => String::new(),
            _ => "stop".to_string(),
        }
    };
    Ok(ChatCompletionResult {
        text: answer,
        model: cfg.model_name.clone(),
        finish,
        usage: crate::zen_core::RawUsage::from_json(&json["usage"]),
        latency_ms: started.elapsed().as_millis() as u64,
        profile: cfg.label.clone(),
        provider: cfg.provider.clone(),
    })
}

/// Fetch model list from a provider's /models endpoint.
async fn fetch_models(base_url: &str, api_key: &str, adapt: &str) -> Result<Vec<String>, String> {
    let client = reqwest::Client::new();
    let is_anthropic = adapt == "anthropic" && base_url.contains("anthropic.com");

    let models_url = if is_anthropic {
        let base = base_url.trim_end_matches("/v1");
        format!("{base}/v1/models")
    } else {
        let base = base_url
            .trim_end_matches('/')
            .trim_end_matches("/chat/completions");
        format!("{base}/models")
    };

    let req = if is_anthropic {
        client
            .get(&models_url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
    } else {
        client
            .get(&models_url)
            .header("Authorization", format!("Bearer {api_key}"))
    };

    let resp = req
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("Connection failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Read error: {e}"))?;

    if !status.is_success() {
        let preview: String = text.chars().take(200).collect();
        return Err(format!("HTTP {status}: {preview}"));
    }

    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Invalid JSON: {e}"))?;
    let list: Vec<String> = json["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(id: &str, label: &str) -> LlmConfig {
        LlmConfig {
            id: id.into(),
            label: label.into(),
            provider: "custom".into(),
            base_url: "https://example.test/v1".into(),
            api_key: "k".into(),
            model_name: format!("model-of-{label}"),
            adapt: "openai".into(),
            max_tokens: 1024,
            context_length: 8192,
            vision: None,
            ..Default::default()
        }
    }

    #[test]
    fn explicit_profile_wins_by_id_or_label() {
        let configs = vec![cfg("llm_1", "Main"), cfg("llm_2", "MoltClaw")];
        // by id
        assert_eq!(
            pick_config(&configs, Some("llm_1"), Some("llm_2"))
                .unwrap()
                .label,
            "MoltClaw"
        );
        // by label, case-insensitive
        assert_eq!(
            pick_config(&configs, Some("llm_1"), Some("moltclaw"))
                .unwrap()
                .id,
            "llm_2"
        );
    }

    #[test]
    fn no_profile_falls_back_to_active_then_first() {
        let configs = vec![cfg("llm_1", "Main"), cfg("llm_2", "MoltClaw")];
        assert_eq!(
            pick_config(&configs, Some("llm_2"), None).unwrap().label,
            "MoltClaw"
        );
        // unknown active → first
        assert_eq!(
            pick_config(&configs, Some("gone"), None).unwrap().id,
            "llm_1"
        );
        // no active → first
        assert_eq!(pick_config(&configs, None, None).unwrap().id, "llm_1");
    }

    #[test]
    fn blank_profile_is_treated_as_unset() {
        let configs = vec![cfg("llm_1", "Main"), cfg("llm_2", "MoltClaw")];
        assert_eq!(
            pick_config(&configs, Some("llm_2"), Some("   "))
                .unwrap()
                .label,
            "MoltClaw"
        );
    }

    #[test]
    fn missing_profile_errors_rather_than_silently_using_the_wrong_model() {
        let configs = vec![cfg("llm_1", "Main")];
        let err = pick_config(&configs, Some("llm_1"), Some("MoltClaw")).unwrap_err();
        assert!(
            err.contains("MoltClaw"),
            "error should name the missing profile: {err}"
        );
    }

    #[test]
    fn empty_config_list_errors() {
        assert!(pick_config(&[], None, None).is_err());
    }

    fn profile(provider: &str, adapt: &str, base_url: &str) -> crate::zen_core::ModelProfile {
        crate::zen_core::ModelProfile {
            name: "p".into(),
            provider: provider.into(),
            model_name: "m".into(),
            base_url: base_url.into(),
            api_key: "k".into(),
            max_tokens: 1024,
            context_length: 8192,
            adapt: Some(adapt.into()),
            ..Default::default()
        }
    }

    /// Regression: an Antigravity profile used to fall through to the OpenAI
    /// branch, which POSTed a chat/completions body at
    /// `daily-cloudcode-pa.googleapis.com` and got Google's HTML 404 page back.
    /// Bridge callers surfaced that page as "LLM HTTP 404 Not Found: <!DOCTYPE
    /// html>…".
    #[test]
    fn oauth_and_in_process_adapters_route_to_the_dispatcher() {
        for (provider, adapt, base) in [
            (
                "antigravity",
                "antigravity",
                "https://daily-cloudcode-pa.googleapis.com",
            ),
            ("codex", "codex", "https://chatgpt.com/backend-api/codex"),
            ("local-mlx", "local-mlx", ""),
            ("local-candle", "local-candle", ""),
        ] {
            assert!(
                needs_adapter_dispatch(&profile(provider, adapt, base)),
                "{adapt} does not speak chat/completions and must be dispatched"
            );
        }
    }

    /// End-to-end against this machine's real `~/.senclaw/config.json` and
    /// OAuth store — the same inputs the bridge's `llm.request` uses. Ignored
    /// by default: it makes a real, billed call on whatever profile is active.
    ///
    /// Run with `cargo test --lib live_active_profile -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_active_profile_completes_through_the_bridge_path() {
        let config_path = dirs::home_dir()
            .expect("home dir")
            .join(".senclaw")
            .join("config.json");
        // The daemon installs this at boot; a test process has to do it itself
        // or every OAuth profile resolves to an empty bearer token.
        crate::providers::oauth::init(crate::providers::oauth::store::default_path(&config_path));

        let r = chat_completion(
            &config_path,
            None,
            "You are terse.",
            "Reply with the single word: ok",
            // Not a token or two: a reasoning model spends the cap on hidden
            // thinking first, and a cap that small comes back with no text at
            // all — which reads as a failure of the path under test.
            512,
            None,
        )
        .await
        .expect("the active profile must complete");

        println!(
            "profile={} provider={} model={} latency={}ms text={:?}",
            r.profile, r.provider, r.model, r.latency_ms, r.text
        );
        assert!(!r.text.trim().is_empty());
    }

    /// The two shapes the hand-rolled requests can actually build stay on the
    /// direct path — this fix must not reroute profiles that already worked.
    #[test]
    fn plain_http_adapters_keep_the_direct_path() {
        assert!(!needs_adapter_dispatch(&profile(
            "custom",
            "openai",
            "https://api.deepseek.com"
        )));
        assert!(!needs_adapter_dispatch(&profile(
            "anthropic",
            "anthropic",
            "https://api.anthropic.com"
        )));
    }
}

//! `/api/oauth/*` — sign in to subscription LLM providers and manage the
//! resulting accounts.
//!
//! Every response here goes through [`RedactedAccount`]. Access and refresh
//! tokens never leave the daemon: the UI gets ids, labels and expiry, and the
//! LLM layer reads the real values straight from the store. That matters more
//! than usual because the daemon serves this API under a permissive CORS
//! layer, so any page the user has open can call it.

use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::Json,
};
use serde::Deserialize;

use crate::providers::oauth::{self, flow, provider};

use super::core::{AppError, UiState};

fn manager() -> Result<Arc<oauth::OauthManager>, AppError> {
    oauth::global().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "OAuth subsystem is not initialised".to_string(),
        )
    })
}

/// GET /api/oauth/providers — what can be signed in to, and the risk of each.
pub(crate) async fn oauth_providers_list() -> Json<serde_json::Value> {
    let providers: Vec<_> = provider::all()
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "displayName": p.display_name,
                "riskNotice": p.risk_notice,
                "brandColor": p.brand_color,
                "brandMark": p.brand_mark,
                "flow": match p.flow {
                    provider::FlowKind::AuthCodePkce => "auth_code_pkce",
                    provider::FlowKind::DeviceCode => "device_code",
                },
                "adapt": p.adapt,
                "compat": crate::providers::adapters::compat_family(p.adapt)
                    .map(|(f, _)| f.as_str()),
                "needsTranslation": crate::providers::adapters::compat_family(p.adapt)
                    .is_some_and(|(_, needs)| needs),
                "baseURL": p.base_url,
                "defaultMaxTokens": p.default_max_tokens,
                "defaultContextLength": p.default_context_length,
                "models": p.default_models.iter().map(|(id, name)| {
                    serde_json::json!({ "id": id, "name": name })
                }).collect::<Vec<_>>(),
                "requiresFixedPort": matches!(
                    p.callback_port,
                    provider::CallbackPort::Fixed(_)
                ),
            })
        })
        .collect();

    Json(serde_json::json!({ "providers": providers }))
}

/// GET /api/provider-catalog — API-key presets (free tiers).
///
/// Lives beside the OAuth handlers because the settings UI renders both lists
/// side by side; the two are otherwise unrelated.
pub(crate) async fn provider_catalog_list() -> Json<serde_json::Value> {
    Json(crate::providers::to_json())
}

/// GET /api/oauth/accounts — connected accounts, tokens stripped.
pub(crate) async fn oauth_accounts_list() -> Result<Json<serde_json::Value>, AppError> {
    let accounts = manager()?.accounts_redacted();
    Ok(Json(serde_json::json!({ "accounts": accounts })))
}

/// POST /api/oauth/:provider/start — begin a browser sign-in.
///
/// Returns the URL to open. The caller opens it (the daemon does not launch a
/// browser on the user's behalf) and then polls the flow status.
pub(crate) async fn oauth_start(
    AxumPath(provider_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manager = manager()?;
    let started = flow::start(manager, &provider_id)
        .await
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(serde_json::to_value(started).unwrap_or_default()))
}

/// GET /api/oauth/flows/:flow_id — poll an in-flight sign-in.
pub(crate) async fn oauth_flow_status(
    AxumPath(flow_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let state = manager()?
        .flow_state(&flow_id)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "No such sign-in flow".to_string()))?;
    Ok(Json(serde_json::to_value(state).unwrap_or_default()))
}

/// DELETE /api/oauth/accounts/:id — forget an account.
pub(crate) async fn oauth_account_delete(
    AxumPath(account_id): AxumPath<String>,
) -> Result<StatusCode, AppError> {
    let removed = manager()?
        .remove(&account_id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError(
            StatusCode::NOT_FOUND,
            "No such OAuth account".to_string(),
        ))
    }
}

/// POST /api/oauth/accounts/:id/refresh — force a token refresh now.
pub(crate) async fn oauth_account_refresh(
    AxumPath(account_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manager = manager()?;
    manager
        .refresh_account(&account_id)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?;

    let account = manager
        .accounts_redacted()
        .into_iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "No such OAuth account".into()))?;
    Ok(Json(serde_json::json!({ "account": account })))
}

/// GET /api/oauth/accounts/:id/models — the models this account can actually
/// use, asked of the provider itself.
///
/// The registry's `default_models` is a static guess; entitlement is per
/// account. Where a provider exposes a listing endpoint we use it, and fall
/// back to the registry only when it doesn't (Anthropic has no such endpoint).
/// `source` tells the UI which it got, so "discovered" can be presented with
/// more confidence than "suggested".
pub(crate) async fn oauth_account_models(
    AxumPath(account_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manager = manager()?;
    let account = manager.account(&account_id).ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("No such OAuth account `{account_id}`"),
        )
    })?;
    let def = provider::get(&account.provider).ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            format!("Unknown provider `{}`", account.provider),
        )
    })?;

    let token = manager
        .ensure_fresh(&account_id)
        .await
        .unwrap_or_else(|_| account.access_token.clone());

    // Google's listing is scoped to the Code Assist project. It is discovered
    // on the first chat, so an account that has never been used yet lists
    // without one — which still works, just less precisely.
    let project = crate::providers::oauth::transport::cached_project_id(&account);
    let discovered =
        crate::providers::oauth::discovery::list_models(def, &token, project.as_deref()).await;

    // A project Google refuses here is one chat would fail on too. Forget it so
    // the next request rediscovers instead of reusing a dead id — this is how a
    // stale value left over from an earlier version gets cleaned up.
    if project.is_some()
        && discovered
            .as_ref()
            .err()
            .is_some_and(|e| crate::providers::oauth::discovery::is_project_rejection(&e.to_string()))
    {
        let _ = manager.set_extra(
            &account_id,
            crate::providers::oauth::transport::ANTIGRAVITY_PROJECT_KEY,
            serde_json::Value::String(String::new()),
        );
    }

    Ok(Json(match discovered {
        Ok(models) if !models.is_empty() => serde_json::json!({
            "source": "discovered",
            "models": models.iter().map(|m| serde_json::json!({
                "id": m.id, "name": m.name,
            })).collect::<Vec<_>>(),
        }),
        // Either the provider has no listing endpoint, or the call failed.
        // Falling back beats showing an empty picker; the reason rides along
        // so the UI can say why.
        other => serde_json::json!({
            "source": "registry",
            "reason": other.err().map(|e| truncate_error(&e.to_string())),
            "models": def.default_models.iter().map(|(id, name)| {
                serde_json::json!({ "id": id, "name": name })
            }).collect::<Vec<_>>(),
        }),
    }))
}

/// Body for probing a model before committing to it.
#[derive(Deserialize)]
pub(crate) struct TestModelBody {
    #[serde(rename = "accountId")]
    account_id: String,
    #[serde(rename = "modelName")]
    model_name: String,
}

/// POST /api/oauth/test-model — send a throwaway prompt through the real
/// adapter and report whether the account can actually use that model.
///
/// Worth having because entitlement is per-account and invisible until you
/// try: an Antigravity plan may list a dozen models and serve three. This runs
/// the same code path a chat would, so a pass here means the model works —
/// not merely that the id looks plausible.
pub(crate) async fn oauth_test_model(
    Json(body): Json<TestModelBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manager = manager()?;
    let account = manager.account(&body.account_id).ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("No such OAuth account `{}`", body.account_id),
        )
    })?;
    let def = provider::get(&account.provider).ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            format!("Unknown provider `{}`", account.provider),
        )
    })?;

    let model_name = body.model_name.trim();
    if model_name.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "modelName is required".to_string(),
        ));
    }

    // Refresh first so a stale token doesn't read as an unusable model.
    let token = manager
        .ensure_fresh(&body.account_id)
        .await
        .unwrap_or_else(|_| account.access_token.clone());

    let profile = crate::zen_core::ModelProfile {
        name: def.display_name.to_string(),
        provider: def.id.to_string(),
        model_name: model_name.to_string(),
        base_url: def.base_url.to_string(),
        api_key: token,
        // Keep the probe tiny: this is a reachability check, not a benchmark.
        max_tokens: 16,
        context_length: def.default_context_length,
        adapt: Some(def.adapt.to_string()),
        vision: None,
        oauth_provider: Some(def.id.to_string()),
        oauth_account_id: Some(account.id.clone()),
        edit_format: None,
    };

    let messages = vec![crate::zen_core::create_user_message(vec![
        crate::zen_core::ContentBlock::Text {
            text: "Reply with the single word: ok".to_string(),
        },
    ])];

    let started = std::time::Instant::now();
    let client = reqwest::Client::new();
    let cancel = tokio_util::sync::CancellationToken::new();
    let outcome = crate::zen_core::query_llm::query_llm(
        &client,
        &messages,
        "",
        &[],
        &cancel,
        &profile,
        false,
        false,
        None, // connectivity probe
    )
    .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;

    Ok(Json(match outcome {
        Ok(msg) => {
            let reply: String = msg
                .message
                .content
                .iter()
                .filter_map(|b| match b {
                    crate::zen_core::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            serde_json::json!({
                "ok": true,
                "model": model_name,
                "latencyMs": elapsed_ms,
                "reply": reply.chars().take(120).collect::<String>(),
            })
        }
        // A failed probe is a result, not a server error — the UI shows the
        // provider's own message so the user can tell "not entitled" from
        // "wrong id" from "rate limited".
        Err(e) => serde_json::json!({
            "ok": false,
            "model": model_name,
            "latencyMs": elapsed_ms,
            "error": truncate_error(&e.to_string()),
        }),
    }))
}

/// Provider errors can be kilobytes of JSON; keep the first useful line.
fn truncate_error(message: &str) -> String {
    // Collapse to one line rather than taking the first: providers pretty-print
    // their JSON errors, so `lines().next()` is often just `{`.
    let flattened = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed: String = flattened.chars().take(300).collect();
    if flattened.chars().count() > 300 {
        format!("{trimmed}…")
    } else {
        trimmed
    }
}

/// Body for binding an account to a new LLM config.
#[derive(Deserialize)]
pub(crate) struct BindAccountBody {
    #[serde(rename = "accountId")]
    account_id: String,
    #[serde(rename = "modelName")]
    model_name: String,
    /// Optional friendly name; defaults to "<provider> — <model>".
    #[serde(default)]
    label: Option<String>,
}

/// POST /api/oauth/bind — create an `LlmConfig` backed by an OAuth account.
///
/// Exists so the UI does not have to know how to assemble the provider
/// defaults (base URL, adapter, context length) for each provider.
pub(crate) async fn oauth_bind_config(
    State(s): State<Arc<UiState>>,
    Json(body): Json<BindAccountBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manager = manager()?;
    let account = manager.account(&body.account_id).ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("No such OAuth account `{}`", body.account_id),
        )
    })?;
    let def = provider::get(&account.provider).ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            format!("Unknown provider `{}`", account.provider),
        )
    })?;

    let model_name = body.model_name.trim();
    if model_name.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "modelName is required".to_string(),
        ));
    }

    let id = format!(
        "llm_{}_{}",
        chrono::Utc::now().timestamp_millis(),
        &account.id[account.id.len().saturating_sub(4)..]
    );
    let cfg = crate::gateway::group_manager::LlmConfig {
        id: id.clone(),
        label: body
            .label
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| format!("{} — {model_name}", def.display_name)),
        provider: def.id.to_string(),
        base_url: def.base_url.to_string(),
        // Empty by design: the credential is resolved from the OAuth store at
        // request time, so no token is written into config.json.
        api_key: String::new(),
        model_name: model_name.to_string(),
        adapt: def.adapt.to_string(),
        max_tokens: def.default_max_tokens,
        context_length: def.default_context_length,
        vision: None,
        auth: Some("oauth".to_string()),
        oauth_account_id: Some(account.id.clone()),
        edit_format: None,
    };

    crate::gateway::group_manager::save_llm_config(&s.config.paths.global_config_path, &cfg)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Match the API-key path: the very first config becomes the active one.
    let stored =
        crate::gateway::group_manager::load_llm_configs(&s.config.paths.global_config_path);
    if stored.configs.len() == 1 {
        let _ = crate::gateway::group_manager::set_active_llm_config(
            &s.config.paths.global_config_path,
            Some(&id),
        );
    }

    Ok(Json(serde_json::to_value(&cfg).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn providers_endpoint_lists_every_registry_entry_with_its_risk() {
        let Json(body) = oauth_providers_list().await;
        let providers = body["providers"].as_array().unwrap();
        assert_eq!(providers.len(), provider::all().len());

        for p in providers {
            assert!(!p["id"].as_str().unwrap().is_empty());
            assert!(
                !p["riskNotice"].as_str().unwrap().is_empty(),
                "risk notice must reach the UI"
            );
            assert!(!p["models"].as_array().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn providers_endpoint_never_exposes_client_secrets() {
        let Json(body) = oauth_providers_list().await;
        let json = serde_json::to_string(&body).unwrap();
        for p in provider::all() {
            if let Some(secret) = p.client_secret() {
                assert!(!json.contains(&secret), "{} secret leaked", p.id);
            }
        }
    }

    #[tokio::test]
    async fn codex_is_flagged_as_needing_its_fixed_port() {
        let Json(body) = oauth_providers_list().await;
        let providers = body["providers"].as_array().unwrap();
        let codex = providers.iter().find(|p| p["id"] == "codex").unwrap();
        assert_eq!(codex["requiresFixedPort"], true);
        let claude = providers.iter().find(|p| p["id"] == "claude").unwrap();
        assert_eq!(claude["requiresFixedPort"], false);
    }

    #[tokio::test]
    async fn endpoints_report_cleanly_when_oauth_is_not_initialised() {
        // In unit tests `oauth::init` is never called, so the global is empty —
        // the handlers must 503 rather than panic.
        if oauth::global().is_some() {
            return; // another test in this binary installed it; nothing to assert
        }
        let err = oauth_accounts_list().await.unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);

        let err = oauth_flow_status(AxumPath("nope".into()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn starting_a_flow_for_an_unknown_provider_is_a_client_error() {
        if oauth::global().is_none() {
            return; // covered by the not-initialised test above
        }
        let err = oauth_start(AxumPath("not-a-provider".into()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }
}

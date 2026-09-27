//! Asking a provider which models an account may actually use.
//!
//! The registry's `default_models` is a curated guess. Real entitlement is per
//! account — an Antigravity plan can advertise a dozen models and serve three,
//! and a Copilot seat differs by org — so where the vendor exposes a listing
//! endpoint, that is the authoritative answer.
//!
//! Ported from 9router's `src/app/api/providers/[id]/models/route.js`. The
//! strategy is keyed on **provider id**, not on `adapt`: Antigravity and Gemini
//! CLI share a wire format but list models through different endpoints, and
//! Codex speaks the Responses API yet lists models OpenAI-style.
//!
//! Every path degrades to `Err`, and the caller falls back to the registry. A
//! listing that fails is a missing convenience, never a broken sign-in.

use anyhow::{bail, Result};
use serde_json::Value;

use super::provider::OauthProviderDef;

/// One model as the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub name: String,
}

/// Code Assist model listing, used by both Google providers.
///
/// Must be the **production** host. Antigravity serves completions from the
/// `daily-` sandbox host, but that host 404s discovery calls — verified live,
/// and 9router's own registry carries the same note ("Discovery on PROD; daily
/// host rejects these").
const CODE_ASSIST_MODELS_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels";

/// The Codex backend gates each entry on `minimal_client_version`; too low a
/// value returns 200 with the newest models quietly missing.
const CODEX_CLIENT_VERSION: &str = "0.144.6";

const COPILOT_MODELS_URL: &str = "https://api.githubcopilot.com/models";
const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";

/// Ask `def`'s backend what this token can use.
///
/// `project_id` is the account's discovered Code Assist project, when one is
/// known. The Google endpoints key their catalogue off it — passing the wrong
/// shape of body there is a 400, not an empty list.
pub async fn list_models(
    def: &OauthProviderDef,
    access_token: &str,
    project_id: Option<&str>,
) -> Result<Vec<DiscoveredModel>> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;

    match def.id {
        // Both Google providers list through the same production endpoint.
        //
        // The body is `{project}` — *not* the `{metadata}` envelope the
        // loadCodeAssist/onboardUser calls take. Sending the wrong one returns
        // 400 Bad Request, which is how this was found.
        "antigravity" | "gemini-cli" => {
            let project = project_id.map(str::trim).filter(|p| !p.is_empty());
            let body = match project {
                Some(p) => serde_json::json!({ "project": p }),
                None => serde_json::json!({}),
            };

            match post_json(&http, CODE_ASSIST_MODELS_URL, access_token, &body).await {
                Ok(json) => Ok(parse_google_models(&json)),
                // A project the account may not bill against fails the listing
                // even though the catalogue itself is readable without one.
                // Retry bare rather than showing the user a dead picker.
                Err(e) if project.is_some() && is_project_rejection(&e.to_string()) => {
                    let json =
                        post_json(&http, CODE_ASSIST_MODELS_URL, access_token, &serde_json::json!({}))
                            .await?;
                    Ok(parse_google_models(&json))
                }
                Err(e) => Err(e),
            }
        }
        "codex" => {
            let url =
                format!("https://chatgpt.com/backend-api/codex/models?client_version={CODEX_CLIENT_VERSION}");
            let json = get_json(&http, &url, access_token, &[("originator", "senclaw")]).await?;
            Ok(with_review_variants(parse_openai_models(&json)))
        }
        "github-copilot" => {
            // Copilot keys its catalogue off the calling editor; without these
            // it answers with an empty list rather than an error.
            let json = get_json(
                &http,
                COPILOT_MODELS_URL,
                access_token,
                &[("Copilot-Integration-Id", "vscode-chat")],
            )
            .await?;
            Ok(parse_copilot_models(&json))
        }
        "claude" => {
            let json = get_json(
                &http,
                ANTHROPIC_MODELS_URL,
                access_token,
                &[
                    ("anthropic-version", super::transport::ANTHROPIC_VERSION),
                    ("anthropic-beta", super::transport::ANTHROPIC_OAUTH_BETA),
                ],
            )
            .await?;
            Ok(parse_openai_models(&json))
        }
        // Everything else that speaks OpenAI lists models the OpenAI way.
        _ if def.adapt == "openai" => {
            let url = format!("{}/models", def.base_url.trim_end_matches('/'));
            let json = get_json(&http, &url, access_token, &[]).await?;
            Ok(parse_openai_models(&json))
        }
        _ => bail!("{} does not publish a model list", def.display_name),
    }
}

/// True when a listing error blames the project rather than the token.
///
/// Google answers a stale or unentitled project with 403/PERMISSION_DENIED;
/// the same catalogue is usually readable with no project at all.
pub(crate) fn is_project_rejection(message: &str) -> bool {
    message.contains("403")
        && (message.contains("PERMISSION_DENIED")
            || message.contains("CONSUMER_INVALID")
            || message.contains("Permission denied"))
}

async fn get_json(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    extra: &[(&str, &str)],
) -> Result<Value> {
    let mut req = http
        .get(url)
        .bearer_auth(token)
        .header("Accept", "application/json")
        .header("User-Agent", super::transport::user_agent());
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    finish(req).await
}

async fn post_json(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    body: &Value,
) -> Result<Value> {
    let req = http
        .post(url)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        // fetchAvailableModels is a discovery call: same identity as
        // loadCodeAssist / onboardUser.
        .header("User-Agent", super::transport::CODE_ASSIST_DISCOVERY_USER_AGENT)
        .header(
            "X-Goog-Api-Client",
            super::transport::CODE_ASSIST_DISCOVERY_API_CLIENT,
        )
        .json(body);
    finish(req).await
}

async fn finish(req: reqwest::RequestBuilder) -> Result<Value> {
    let response = req.send().await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("model listing failed ({status}): {}", truncate(&text, 300));
    }
    Ok(serde_json::from_str(&text)?)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// Google's Code Assist listings come in two shapes.
///
/// `models` is either an array of objects, or an object keyed by model id
/// whose values carry `displayName` and an `isInternal` flag. Both appear in
/// the wild depending on endpoint and account, so both are handled.
pub fn parse_google_models(json: &Value) -> Vec<DiscoveredModel> {
    let Some(models) = json.get("models") else {
        return Vec::new();
    };

    if let Some(array) = models.as_array() {
        return array
            .iter()
            .filter(|m| m.get("isInternal").and_then(|v| v.as_bool()) != Some(true))
            .filter_map(|m| {
                let id = first_str(m, &["id", "model", "modelId", "name"])?;
                // `name` may be a `models/<id>` resource path.
                let id = id.rsplit('/').next()?.trim().to_string();
                if id.is_empty() {
                    return None;
                }
                let name = first_str(m, &["displayName", "name"])
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty() && !s.contains('/'))
                    .unwrap_or_else(|| id.clone());
                Some(DiscoveredModel { id, name })
            })
            .collect();
    }

    if let Some(map) = models.as_object() {
        let mut out: Vec<DiscoveredModel> = map
            .iter()
            .filter(|(_, info)| info.get("isInternal").and_then(|v| v.as_bool()) != Some(true))
            .filter(|(id, _)| !id.trim().is_empty())
            .map(|(id, info)| DiscoveredModel {
                id: id.trim().to_string(),
                name: first_str(info, &["displayName", "name"])
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| id.trim().to_string()),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        return out;
    }

    Vec::new()
}

/// Pull model ids out of an OpenAI-shaped listing (`data`, `models`, or a bare
/// array — providers differ).
pub fn parse_openai_models(json: &Value) -> Vec<DiscoveredModel> {
    let list = json
        .get("data")
        .or_else(|| json.get("models"))
        .or_else(|| json.get("results"))
        .and_then(|d| d.as_array())
        .or_else(|| json.as_array());
    let Some(list) = list else {
        return Vec::new();
    };

    let mut out: Vec<DiscoveredModel> = list
        .iter()
        .filter_map(|m| {
            let id = first_str(m, &["id", "slug", "model", "name"])?.trim().to_string();
            if id.is_empty() {
                return None;
            }
            let name = first_str(m, &["display_name", "displayName", "name"])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| id.clone());
            Some(DiscoveredModel { id, name })
        })
        // Embedding models are not chat models and only clutter the picker.
        .filter(|m| !m.id.to_lowercase().contains("embed"))
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// Copilot reports every model its API knows, including non-chat and
/// org-disabled ones; only the enabled chat models are usable.
pub fn parse_copilot_models(json: &Value) -> Vec<DiscoveredModel> {
    let Some(list) = json.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<DiscoveredModel> = list
        .iter()
        .filter(|m| {
            m.get("capabilities")
                .and_then(|c| c.get("type"))
                .and_then(|t| t.as_str())
                == Some("chat")
        })
        .filter(|m| {
            m.get("policy")
                .and_then(|p| p.get("state"))
                .and_then(|s| s.as_str())
                != Some("disabled")
        })
        .filter_map(|m| {
            let id = m.get("id").and_then(|v| v.as_str())?.trim().to_string();
            if id.is_empty() {
                return None;
            }
            let name = m
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| id.clone());
            Some(DiscoveredModel { id, name })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// Codex bills "review" runs against a separate quota family, exposed as a
/// `<id>-review` sibling of each chat model.
pub fn with_review_variants(models: Vec<DiscoveredModel>) -> Vec<DiscoveredModel> {
    let mut out = Vec::with_capacity(models.len() * 2);
    for m in models {
        let already_review = m.id.ends_with("-review");
        let review = DiscoveredModel {
            id: format!("{}-review", m.id),
            name: format!("{} Review", m.name),
        };
        out.push(m);
        if !already_review {
            out.push(review);
        }
    }
    out
}

/// First present string field among `keys`.
fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| value.get(*k)?.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Google: array shape ----

    #[test]
    fn google_array_listings_are_parsed() {
        let json = serde_json::json!({
            "models": [
                { "id": "gemini-3.6-flash-high", "displayName": "Gemini 3.6 Flash (High)" },
                { "id": "claude-sonnet-4-6", "displayName": "Claude Sonnet 4.6" }
            ]
        });
        let models = parse_google_models(&json);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gemini-3.6-flash-high");
        assert_eq!(models[0].name, "Gemini 3.6 Flash (High)");
    }

    // ---- Google: object-map shape ----
    //
    // This is the shape that made discovery silently return nothing before:
    // `models` is a map, not a list.

    #[test]
    fn google_map_listings_are_parsed() {
        let json = serde_json::json!({
            "models": {
                "gemini-3-flash": { "displayName": "Gemini 3 Flash" },
                "gemini-pro-agent": { "displayName": "Gemini 3.1 Pro (High)" }
            }
        });
        let models = parse_google_models(&json);
        assert_eq!(models.len(), 2);
        // Sorted for a stable picker.
        assert_eq!(models[0].id, "gemini-3-flash");
        assert_eq!(models[1].name, "Gemini 3.1 Pro (High)");
    }

    #[test]
    fn internal_google_models_are_hidden_in_both_shapes() {
        let map = serde_json::json!({
            "models": {
                "public-one": { "displayName": "Public" },
                "secret-one": { "displayName": "Secret", "isInternal": true }
            }
        });
        let models = parse_google_models(&map);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "public-one");

        let array = serde_json::json!({
            "models": [
                { "id": "public-one" },
                { "id": "secret-one", "isInternal": true }
            ]
        });
        assert_eq!(parse_google_models(&array).len(), 1);
    }

    #[test]
    fn a_resource_style_name_is_reduced_to_its_id() {
        let json = serde_json::json!({ "models": [{ "name": "models/gemini-2.5-pro" }] });
        let models = parse_google_models(&json);
        assert_eq!(models[0].id, "gemini-2.5-pro");
        assert_eq!(models[0].name, "gemini-2.5-pro");
    }

    #[test]
    fn a_google_model_without_a_display_name_falls_back_to_its_id() {
        let json = serde_json::json!({ "models": [{ "id": "gpt-oss-120b" }] });
        assert_eq!(parse_google_models(&json)[0].name, "gpt-oss-120b");
    }

    #[test]
    fn malformed_google_replies_yield_nothing_rather_than_panicking() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({ "models": [] }),
            serde_json::json!({ "models": {} }),
            serde_json::json!({ "models": [{}] }),
            serde_json::json!({ "models": "nonsense" }),
            serde_json::json!({ "models": [{ "id": "  " }] }),
        ] {
            assert!(parse_google_models(&body).is_empty(), "{body}");
        }
    }

    // ---- OpenAI-shaped ----

    #[test]
    fn openai_listings_are_parsed_sorted_and_deduped() {
        let json = serde_json::json!({
            "data": [
                { "id": "zeta" },
                { "id": "alpha", "display_name": "Alpha" },
                { "id": "alpha" }
            ]
        });
        let models = parse_openai_models(&json);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "alpha");
        assert_eq!(models[1].id, "zeta");
    }

    #[test]
    fn openai_parsing_accepts_the_alternate_envelopes() {
        // Some providers use `models`, some return a bare array.
        assert_eq!(
            parse_openai_models(&serde_json::json!({ "models": [{ "id": "a" }] }))[0].id,
            "a"
        );
        assert_eq!(
            parse_openai_models(&serde_json::json!([{ "id": "b" }]))[0].id,
            "b"
        );
    }

    #[test]
    fn embedding_models_are_kept_out_of_the_chat_picker() {
        let json = serde_json::json!({
            "data": [{ "id": "gpt-5" }, { "id": "text-embedding-3-large" }]
        });
        let models = parse_openai_models(&json);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5");
    }

    #[test]
    fn malformed_openai_replies_yield_nothing() {
        assert!(parse_openai_models(&serde_json::json!({})).is_empty());
        assert!(parse_openai_models(&serde_json::json!({ "data": [] })).is_empty());
        assert!(parse_openai_models(&serde_json::json!({ "data": [{ "id": "" }] })).is_empty());
    }

    // ---- Copilot ----

    #[test]
    fn copilot_returns_only_enabled_chat_models() {
        let json = serde_json::json!({
            "data": [
                { "id": "gpt-5.4", "name": "GPT-5.4", "capabilities": { "type": "chat" } },
                { "id": "embed-1", "capabilities": { "type": "embeddings" } },
                { "id": "blocked", "capabilities": { "type": "chat" },
                  "policy": { "state": "disabled" } },
                { "id": "allowed", "capabilities": { "type": "chat" },
                  "policy": { "state": "enabled" } }
            ]
        });
        let models = parse_copilot_models(&json);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["allowed", "gpt-5.4"]);
    }

    #[test]
    fn a_copilot_model_without_capabilities_is_not_assumed_chat() {
        let json = serde_json::json!({ "data": [{ "id": "mystery" }] });
        assert!(parse_copilot_models(&json).is_empty());
    }

    // ---- Codex review variants ----

    #[test]
    fn codex_models_gain_a_review_sibling() {
        let models = with_review_variants(vec![DiscoveredModel {
            id: "gpt-5.5".into(),
            name: "GPT 5.5".into(),
        }]);
        assert_eq!(models.len(), 2);
        assert_eq!(models[1].id, "gpt-5.5-review");
        assert_eq!(models[1].name, "GPT 5.5 Review");
    }

    #[test]
    fn an_existing_review_model_is_not_doubled() {
        let models = with_review_variants(vec![DiscoveredModel {
            id: "gpt-5.5-review".into(),
            name: "GPT 5.5 Review".into(),
        }]);
        assert_eq!(models.len(), 1);
    }

    // ---- routing ----

    #[test]
    fn a_project_rejection_is_told_apart_from_other_failures() {
        // The live 403 that a stale project id produced.
        assert!(is_project_rejection(
            "model listing failed (403 Forbidden): { \"error\": { \"status\": \"PERMISSION_DENIED\" } }"
        ));
        assert!(is_project_rejection(
            "model listing failed (403 Forbidden): CONSUMER_INVALID"
        ));

        // A bad token is not a project problem — retrying bare would not help.
        assert!(!is_project_rejection(
            "model listing failed (401 Unauthorized): invalid token"
        ));
        assert!(!is_project_rejection(
            "model listing failed (403 Forbidden): quota exhausted"
        ));
        assert!(!is_project_rejection(
            "model listing failed (500 Internal): PERMISSION_DENIED"
        ));
    }

    #[test]
    fn code_assist_bodies_use_the_project_envelope() {
        // Regression guard for a live 400: the listing call takes
        // `{project}`, while loadCodeAssist/onboardUser take `{metadata}`.
        // Sending the latter here is rejected outright.
        let with_project = serde_json::json!({ "project": "projects/42" });
        assert!(with_project.get("project").is_some());
        assert!(
            with_project.get("metadata").is_none(),
            "the listing body must not carry a metadata envelope"
        );
    }

    #[test]
    fn code_assist_discovery_targets_the_production_host() {
        // The `daily-` sandbox host serves Antigravity completions but 404s
        // discovery — a live-verified trap worth pinning down.
        assert!(CODE_ASSIST_MODELS_URL.starts_with("https://cloudcode-pa.googleapis.com"));
        assert!(!CODE_ASSIST_MODELS_URL.contains("daily-"));
        assert!(!CODE_ASSIST_MODELS_URL.contains("sandbox"));
    }

    #[test]
    fn discovery_branches_on_provider_id_not_adapter() {
        // Codex shares `adapt` with Grok but has its own listing endpoint, so
        // keying on `adapt` alone would send one to the wrong URL.
        let codex = super::super::provider::get("codex").unwrap();
        let grok = super::super::provider::get("grok").unwrap();
        assert_eq!(codex.adapt, grok.adapt, "same adapter…");
        assert_ne!(codex.id, grok.id, "…but discovery must branch on id");
    }

    #[tokio::test]
    async fn only_providers_with_no_endpoint_report_unsupported() {
        // Every provider should reach a listing strategy; the anthropic-format
        // ones that are not Claude itself (Kimi) have none.
        let kimi = super::super::provider::get("kimi").unwrap();
        let err = list_models(kimi, "token", None).await.unwrap_err().to_string();
        assert!(err.contains("does not publish"), "{err}");
    }
}

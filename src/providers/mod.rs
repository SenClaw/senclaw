//! Catalog of ready-made LLM provider presets.
//!
//! These are not a new auth mechanism — every entry maps onto the `LlmConfig`
//! SenClaw already has (`base_url` + `api_key` + `adapt`). The catalog exists
//! so the UI can offer "pick a provider" instead of "paste a base URL", and so
//! the endpoints live in one reviewed place rather than in a dropdown in the
//! frontend.
//!
//! Ported from 9router's `free` / `freeTier` provider registry. Entries whose
//! upstream needs a bespoke wire format (Kiro, Gemini CLI, Vertex) are
//! deliberately absent: they would need an adapter, not a preset, and a
//! preset that silently fails is worse than no preset. Media-only providers
//! (TTS, image, search) are out of scope for the chat model picker.
//!
//! Deliberately excluded for a different reason: OpenRouter and Ollama already
//! have first-class entries in the web UI's provider list, so duplicating them
//! here would give the user two ways to configure the same thing.

pub mod adapters;
pub mod oauth;

/// How a preset authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeAuth {
    /// Needs a key the user obtains from `signup_url`.
    ApiKey,
    /// Open endpoint — no credential at all.
    None,
}

/// One provider preset.
#[derive(Debug, Clone)]
pub struct FreeProviderDef {
    pub id: &'static str,
    pub display_name: &'static str,
    /// Base URL *without* the `/chat/completions` suffix — the OpenAI adapter
    /// appends that itself.
    pub base_url: &'static str,
    pub adapt: &'static str,
    pub auth: FreeAuth,
    /// Where the user gets a key. `None` for open endpoints.
    pub signup_url: Option<&'static str>,
    /// One line shown under the provider in the picker.
    pub note: &'static str,
    /// Brand colour (hex) for the UI badge.
    pub brand_color: &'static str,
    /// Badge monogram — a clean initial rather than a traced vendor logo.
    pub brand_mark: &'static str,
    /// Placeholder inside `base_url` the user must fill in before the preset
    /// works (Cloudflare needs an account id in the path).
    pub url_placeholder: Option<&'static str>,
    pub models: &'static [(&'static str, &'static str)],
    pub default_max_tokens: u32,
    pub default_context_length: u32,
}

impl FreeProviderDef {
    /// True when [`Self::base_url`] still contains an unfilled placeholder.
    pub fn needs_url_substitution(&self) -> bool {
        self.url_placeholder.is_some()
    }

    /// Replace the placeholder with a user-supplied value.
    pub fn resolve_base_url(&self, value: &str) -> String {
        match self.url_placeholder {
            Some(field) => self.base_url.replace(&format!("{{{field}}}"), value.trim()),
            None => self.base_url.to_string(),
        }
    }
}

static CATALOG: &[FreeProviderDef] = &[
    FreeProviderDef {
        id: "google-ai-studio",
        display_name: "Google AI Studio",
        // Google ships an OpenAI-compatible surface alongside its native API,
        // which is why this is a preset and not a `gemini` adapter.
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        adapt: "openai",
        auth: FreeAuth::ApiKey,
        signup_url: Some("https://aistudio.google.com/apikey"),
        note: "Generous free tier on Gemini. Key from AI Studio, no billing needed.",
        brand_color: "#4285F4",
        brand_mark: "G",
        url_placeholder: None,
        models: &[
            ("gemini-2.5-flash", "Gemini 2.5 Flash"),
            ("gemini-2.5-pro", "Gemini 2.5 Pro"),
            ("gemini-2.0-flash", "Gemini 2.0 Flash"),
        ],
        default_max_tokens: 8192,
        default_context_length: 1_000_000,
    },
    FreeProviderDef {
        id: "bazaarlink",
        display_name: "BazaarLink",
        base_url: "https://bazaarlink.ai/api/v1",
        adapt: "openai",
        auth: FreeAuth::ApiKey,
        signup_url: Some("https://bazaarlink.ai"),
        note: "Aggregator with a zero-cost `auto:free` route.",
        brand_color: "#7C3AED",
        brand_mark: "BL",
        url_placeholder: None,
        models: &[
            ("auto:free", "Auto Free (Zero Cost)"),
            ("claude-sonnet-4.6", "Claude Sonnet 4.6"),
            ("claude-haiku-4.5", "Claude Haiku 4.5"),
            ("gpt-5.5", "GPT-5.5"),
            ("gpt-5.4", "GPT-5.4"),
        ],
        default_max_tokens: 8192,
        default_context_length: 200_000,
    },
    FreeProviderDef {
        id: "kilo-gateway",
        display_name: "Kilo Gateway",
        base_url: "https://api.kilo.ai/api/gateway",
        adapt: "openai",
        auth: FreeAuth::ApiKey,
        signup_url: Some("https://kilo.ai/dashboard?tab=apiKeys"),
        note: "Free tier includes the Nemotron and Kat Coder `:free` models.",
        brand_color: "#F97316",
        brand_mark: "KG",
        url_placeholder: None,
        models: &[
            ("kilo-auto/free", "Kilo Auto Free"),
            (
                "nvidia/nemotron-3-super-120b-a12b:free",
                "Nemotron 3 Super 120B (Free)",
            ),
            (
                "nvidia/nemotron-3-ultra-550b-a55b:free",
                "Nemotron 3 Ultra 550B (Free)",
            ),
            (
                "kwaipilot/kat-coder-pro-v2.5:free",
                "Kat Coder Pro v2.5 (Free)",
            ),
        ],
        default_max_tokens: 8192,
        default_context_length: 128_000,
    },
    FreeProviderDef {
        id: "nvidia",
        display_name: "NVIDIA NIM",
        base_url: "https://integrate.api.nvidia.com/v1",
        adapt: "openai",
        auth: FreeAuth::ApiKey,
        signup_url: Some("https://build.nvidia.com/settings/api-keys"),
        note: "Free developer credits on build.nvidia.com.",
        brand_color: "#76B900",
        brand_mark: "NV",
        url_placeholder: None,
        models: &[
            ("deepseek-ai/deepseek-v4-flash", "DeepSeek V4 Flash"),
            ("deepseek-ai/deepseek-v4-pro", "DeepSeek V4 Pro"),
            ("minimaxai/minimax-m3", "MiniMax M3"),
            ("z-ai/glm-5.2", "GLM 5.2"),
            ("moonshotai/kimi-k2.6", "Kimi K2.6"),
        ],
        default_max_tokens: 8192,
        default_context_length: 128_000,
    },
    FreeProviderDef {
        id: "kimchi",
        display_name: "Kimchi",
        base_url: "https://llm.kimchi.dev/openai/v1",
        adapt: "openai",
        auth: FreeAuth::ApiKey,
        signup_url: Some("https://app.kimchi.dev"),
        note: "Free access to the Kimi and MiniMax families.",
        brand_color: "#EF4444",
        brand_mark: "KC",
        url_placeholder: None,
        models: &[
            ("kimi-k2.7", "Kimi K2.7"),
            ("kimi-k2.6", "Kimi K2.6"),
            ("minimax-m3", "MiniMax M3"),
            ("nemotron-3-ultra-fp4", "Nemotron 3 Ultra FP4"),
        ],
        default_max_tokens: 8192,
        default_context_length: 200_000,
    },
    FreeProviderDef {
        id: "byteplus",
        display_name: "BytePlus Ark",
        base_url: "https://ark.ap-southeast.bytepluses.com/api/coding/v3",
        adapt: "openai",
        auth: FreeAuth::ApiKey,
        signup_url: Some("https://console.byteplus.com/ark"),
        note: "Seed 2.0 coding models; free quota on the coding endpoint.",
        brand_color: "#3B82F6",
        brand_mark: "BP",
        url_placeholder: None,
        models: &[
            ("seed-2-0-pro-260328", "Seed 2.0 Pro"),
            ("seed-2-0-code-preview-260328", "Seed 2.0 Code Preview"),
            ("seed-2-0-mini-260215", "Seed 2.0 Mini"),
            ("kimi-k2-thinking-251104", "Kimi K2 Thinking"),
        ],
        default_max_tokens: 8192,
        default_context_length: 256_000,
    },
    FreeProviderDef {
        id: "llm7",
        display_name: "LLM7",
        base_url: "https://api.llm7.io/v1",
        adapt: "openai",
        auth: FreeAuth::ApiKey,
        signup_url: Some("https://llm7.io"),
        note: "Free relay across several frontier models.",
        brand_color: "#14B8A6",
        brand_mark: "L7",
        url_placeholder: None,
        models: &[
            ("gpt-5.5", "GPT-5.5"),
            ("claude-opus-5", "Claude Opus 5"),
            ("deepseek-v4-flash", "DeepSeek V4 Flash"),
            ("grok-4.5", "Grok 4.5"),
            ("kimi-k3", "Kimi K3"),
        ],
        default_max_tokens: 8192,
        default_context_length: 128_000,
    },
    FreeProviderDef {
        id: "api-airforce",
        display_name: "API Airforce",
        base_url: "https://api.airforce/v1",
        adapt: "openai",
        auth: FreeAuth::ApiKey,
        signup_url: Some("https://api.airforce"),
        note: "Small free relay; rate limits are tight.",
        brand_color: "#64748B",
        brand_mark: "AF",
        url_placeholder: None,
        models: &[
            ("anthropic/claude-3.7-sonnet", "Claude 3.7 Sonnet (Free)"),
            ("moonshot/kimi-k2.6", "Kimi K2.6 (Free)"),
            ("google/gemini-2.5-flash", "Gemini 2.5 Flash (Free)"),
        ],
        default_max_tokens: 4096,
        default_context_length: 128_000,
    },
    FreeProviderDef {
        id: "poolside",
        display_name: "Poolside",
        base_url: "https://inference.poolside.ai/v1",
        adapt: "openai",
        auth: FreeAuth::ApiKey,
        signup_url: Some("https://platform.poolside.ai/api-keys"),
        note: "Poolside's own Laguna coding models.",
        brand_color: "#0891B2",
        brand_mark: "PS",
        url_placeholder: None,
        models: &[
            ("poolside/laguna-s-2.1", "Laguna S 2.1"),
            ("poolside/laguna-xs-2.1", "Laguna XS 2.1"),
        ],
        default_max_tokens: 8192,
        default_context_length: 128_000,
    },
    FreeProviderDef {
        id: "cloudflare-ai",
        display_name: "Cloudflare Workers AI",
        base_url: "https://api.cloudflare.com/client/v4/accounts/{accountId}/ai/v1",
        adapt: "openai",
        auth: FreeAuth::ApiKey,
        signup_url: Some("https://dash.cloudflare.com/profile/api-tokens"),
        note: "Daily free neuron allowance. Needs your Cloudflare account id in the URL.",
        brand_color: "#F6821F",
        brand_mark: "CF",
        url_placeholder: Some("accountId"),
        models: &[
            (
                "@cf/meta/llama-3.1-8b-instruct-fp8-fast",
                "Llama 3.1 8B Fast",
            ),
            ("@cf/meta/llama-3.2-3b-instruct", "Llama 3.2 3B Instruct"),
            (
                "@cf/mistralai/mistral-small-3.1-24b-instruct",
                "Mistral Small 3.1 24B",
            ),
        ],
        default_max_tokens: 4096,
        default_context_length: 128_000,
    },
    FreeProviderDef {
        id: "mimo-free",
        display_name: "Xiaomi MiMo (open)",
        base_url: "https://api.xiaomimimo.com/api/free-ai/openai",
        adapt: "openai",
        auth: FreeAuth::None,
        signup_url: None,
        note: "No credential required. Availability is best-effort.",
        brand_color: "#FF6900",
        brand_mark: "Mi",
        url_placeholder: None,
        models: &[("mimo-auto", "MiMo Auto")],
        default_max_tokens: 4096,
        default_context_length: 128_000,
    },
];

/// Every preset in the catalog.
pub fn all() -> &'static [FreeProviderDef] {
    CATALOG
}

/// Look up a preset by id.
pub fn get(id: &str) -> Option<&'static FreeProviderDef> {
    CATALOG.iter().find(|p| p.id == id)
}

/// Catalog as JSON for the settings UI.
pub fn to_json() -> serde_json::Value {
    let providers: Vec<_> = CATALOG
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "displayName": p.display_name,
                "baseURL": p.base_url,
                "adapt": p.adapt,
                "compat": adapters::compat_family(p.adapt).map(|(f, _)| f.as_str()),
                "auth": match p.auth {
                    FreeAuth::ApiKey => "api_key",
                    FreeAuth::None => "none",
                },
                "signupUrl": p.signup_url,
                "note": p.note,
                "brandColor": p.brand_color,
                "brandMark": p.brand_mark,
                "urlPlaceholder": p.url_placeholder,
                "defaultMaxTokens": p.default_max_tokens,
                "defaultContextLength": p.default_context_length,
                "models": p.models.iter().map(|(id, name)| {
                    serde_json::json!({ "id": id, "name": name })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::json!({ "providers": providers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<_> = all().iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate preset id");
    }

    #[test]
    fn every_preset_is_addressable() {
        for p in all() {
            assert!(get(p.id).is_some(), "{}", p.id);
        }
        assert!(get("nope").is_none());
    }

    #[test]
    fn base_urls_omit_the_chat_completions_suffix() {
        // The OpenAI adapter appends it; a preset that includes it would
        // produce `/chat/completions/chat/completions`.
        for p in all() {
            assert!(
                !p.base_url.contains("/chat/completions"),
                "{} carries the suffix",
                p.id
            );
        }
    }

    #[test]
    fn base_urls_are_https_and_unslashed() {
        for p in all() {
            assert!(p.base_url.starts_with("https://"), "{}", p.id);
            assert!(!p.base_url.ends_with('/'), "{} has a trailing slash", p.id);
        }
    }

    #[test]
    fn every_preset_offers_at_least_one_model() {
        for p in all() {
            assert!(!p.models.is_empty(), "{} has no models", p.id);
            for (id, name) in p.models {
                assert!(!id.is_empty() && !name.is_empty(), "{}", p.id);
            }
        }
    }

    #[test]
    fn key_based_presets_tell_the_user_where_to_get_one() {
        for p in all() {
            match p.auth {
                FreeAuth::ApiKey => {
                    assert!(p.signup_url.is_some(), "{} has no signup URL", p.id)
                }
                FreeAuth::None => assert!(p.signup_url.is_none(), "{}", p.id),
            }
        }
    }

    #[test]
    fn only_declared_placeholders_appear_in_urls() {
        for p in all() {
            let has_brace = p.base_url.contains('{');
            assert_eq!(
                has_brace,
                p.needs_url_substitution(),
                "{} placeholder mismatch",
                p.id
            );
        }
    }

    #[test]
    fn placeholder_substitution_produces_a_clean_url() {
        let cf = get("cloudflare-ai").unwrap();
        assert!(cf.needs_url_substitution());
        let resolved = cf.resolve_base_url("  acct-123  ");
        assert_eq!(
            resolved,
            "https://api.cloudflare.com/client/v4/accounts/acct-123/ai/v1"
        );
        assert!(!resolved.contains('{'));
    }

    #[test]
    fn substitution_is_a_no_op_for_plain_presets() {
        let p = get("nvidia").unwrap();
        assert_eq!(p.resolve_base_url("ignored"), p.base_url);
    }

    #[test]
    fn json_shape_matches_the_catalog() {
        let json = to_json();
        let providers = json["providers"].as_array().unwrap();
        assert_eq!(providers.len(), all().len());

        let mimo = providers.iter().find(|p| p["id"] == "mimo-free").unwrap();
        assert_eq!(mimo["auth"], "none");
        assert!(mimo["signupUrl"].is_null());

        let cf = providers
            .iter()
            .find(|p| p["id"] == "cloudflare-ai")
            .unwrap();
        assert_eq!(cf["auth"], "api_key");
        assert_eq!(cf["urlPlaceholder"], "accountId");
    }

    #[test]
    fn presets_that_would_need_a_custom_wire_format_are_absent() {
        // These exist in 9router but need an adapter, not a preset. Guard
        // against someone adding them as plain OpenAI entries.
        for banned in ["kiro", "gemini-cli", "vertex", "devin-cli"] {
            assert!(get(banned).is_none(), "{banned} needs an adapter");
        }
    }

    #[test]
    fn presets_do_not_duplicate_first_class_providers() {
        for banned in ["openrouter", "ollama"] {
            assert!(
                get(banned).is_none(),
                "{banned} already has a first-class UI entry"
            );
        }
    }
}

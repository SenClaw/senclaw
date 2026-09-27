//! Config types for group_manager module.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct GlobalAgentConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) allowed_work_dirs: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct GroupConfigEntry {
    pub(super) jid: String,
    pub(super) folder: String,
    pub(super) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) group_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) requires_trigger: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) allowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) allowed_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) allowed_work_dirs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) bot_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) max_messages: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "llmConfigId"
    )]
    pub(super) llm_config_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuAppConfig {
    #[serde(rename = "appSecret")]
    pub app_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WechatAccountConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramBotConfig {
    pub token: String,
    #[serde(rename = "adminUserId")]
    pub admin_user_id: String,
    pub folder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// User-set default flow handlers + widget disable list — the `defaults`
/// section of `~/.senclaw/config.json`, edited from Plugins → Widget.
///
/// Every field is optional so the file round-trips; [`Self::effective_*`]
/// resolve the hard-coded fallbacks (which match today's behavior exactly, so
/// an absent section changes nothing).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DefaultsConfig {
    /// Where a link opens: `system-browser` | `mini-browser` | `new-tab`.
    #[serde(rename = "openLink", default, skip_serializing_if = "Option::is_none")]
    pub open_link: Option<String>,
    /// How media plays: `inline-widget` | `mini-browser` | `system-browser`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
    /// Which search the agent should prefer: `browser` | `search-app`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// Engine for `browser_search`: `google` | `bing`.
    #[serde(
        rename = "searchEngine",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub search_engine: Option<String>,
    /// Default note store: `space-notes` | `wiki` | `memory`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Full widget ids (`chart`, `crm.pipeline`, …) the user switched off.
    #[serde(
        rename = "disabledWidgets",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub disabled_widgets: Option<Vec<String>>,
}

impl DefaultsConfig {
    pub fn effective_open_link(&self) -> &str {
        self.open_link.as_deref().unwrap_or("system-browser")
    }
    pub fn effective_media(&self) -> &str {
        self.media.as_deref().unwrap_or("inline-widget")
    }
    pub fn effective_search(&self) -> &str {
        self.search.as_deref().unwrap_or("browser")
    }
    pub fn effective_search_engine(&self) -> &str {
        self.search_engine.as_deref().unwrap_or("google")
    }
    pub fn effective_note(&self) -> &str {
        self.note.as_deref().unwrap_or("space-notes")
    }

    /// Render the `## User defaults` block injected into the agent system
    /// prompt. `None` when the user has configured nothing — the prompt must
    /// stay byte-identical to today's for untouched installs.
    pub fn render_prompt_block(&self) -> Option<String> {
        let mut lines: Vec<String> = Vec::new();
        match self.search.as_deref() {
            Some("search-app") => lines.push(
                "- Search: prefer `mcp__search-mcp__search_query` (the Search app — federated \
                 web + knowledge + wiki). Fall back to `browser_search` only if that tool is \
                 unavailable."
                    .to_string(),
            ),
            Some("browser") => {
                if let Some(engine) = self.search_engine.as_deref() {
                    lines.push(format!(
                        "- Search: use `browser_search` with engine \"{engine}\"."
                    ));
                }
            }
            _ => {
                if let Some(engine) = self.search_engine.as_deref() {
                    lines.push(format!(
                        "- Search: use `browser_search` with engine \"{engine}\"."
                    ));
                }
            }
        }
        match self.note.as_deref() {
            Some("wiki") => lines.push(
                "- Notes: when the user asks to note/save something, store it in the wiki via \
                 `wiki_write` (their default note store)."
                    .to_string(),
            ),
            Some("memory") => lines.push(
                "- Notes: when the user asks to note/save something, store it via `memory_save` \
                 (their default note store)."
                    .to_string(),
            ),
            Some("space-notes") => lines.push(
                "- Notes: when the user asks to note/save something, use `space_note_create` \
                 (their default note store)."
                    .to_string(),
            ),
            _ => {}
        }
        match self.media.as_deref() {
            Some("inline-widget") => lines.push(
                "- Media: when asked to play a video/audio URL, use `emit_widget` (kind `video` \
                 or `audio`) so it plays inline in the chat."
                    .to_string(),
            ),
            Some("mini-browser") => lines.push(
                "- Media: when asked to play a video/audio URL, open it in the Mini Browser app \
                 (`mcp__mini-browser-mcp__browser_navigate`)."
                    .to_string(),
            ),
            _ => {}
        }
        if self.open_link.as_deref() == Some("mini-browser") {
            lines.push(
                "- Links: when asked to open a URL for the user, open it in the Mini Browser app \
                 (`mcp__mini-browser-mcp__browser_navigate`) instead of the system browser."
                    .to_string(),
            );
        }
        if lines.is_empty() {
            None
        } else {
            Some(format!("## User defaults\n{}", lines.join("\n")))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// Provider: "openai" | "openrouter" | "ollama" | "local" | "none"
    pub provider: String,
    #[serde(rename = "apiKey", default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    #[serde(rename = "baseURL", default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(
        rename = "modelName",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub model_name: String,
    /// Local model path (only for provider="local")
    #[serde(
        rename = "modelPath",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub model_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmConfig {
    pub id: String,
    pub label: String,
    pub provider: String,
    #[serde(rename = "baseURL")]
    pub base_url: String,
    #[serde(rename = "apiKey")]
    pub api_key: String,
    #[serde(rename = "modelName")]
    pub model_name: String,
    /// "openai" or "anthropic"
    pub adapt: String,
    #[serde(rename = "maxTokens")]
    pub max_tokens: u32,
    #[serde(rename = "contextLength")]
    pub context_length: u32,
    /// Explicitly declare whether vision input is supported; undefined = auto-infer from modelName
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// Per-model edit format for the `Edit` tool (`exact` | `fuzzy` | `udiff`
    /// | `whole`); absent = exact.
    #[serde(rename = "editFormat", default, skip_serializing_if = "Option::is_none")]
    pub edit_format: Option<crate::zen_core::EditFormat>,

    /// How this config authenticates: `None`/`"api_key"` uses [`Self::api_key`],
    /// `"oauth"` resolves a token from the OAuth account store instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// Account id in `oauth.json` when [`Self::auth`] is `"oauth"`.
    ///
    /// Only the *id* lives here. Token material deliberately stays out of
    /// `config.json`, which is world-readable through `GET /api/llm-config`.
    #[serde(
        rename = "oauthAccountId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub oauth_account_id: Option<String>,
}

impl LlmConfig {
    /// True when this config draws its credential from the OAuth store.
    pub fn is_oauth(&self) -> bool {
        self.auth.as_deref() == Some("oauth")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct AdminPermissionsSection {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "skipMainAgentPermissions"
    )]
    pub(super) skip_main_agent_permissions: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "skipAllAgentsPermissions"
    )]
    pub(super) skip_all_agents_permissions: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct GlobalConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agents: Option<HashMap<String, GlobalAgentConfig>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "adminPermissions"
    )]
    pub(super) admin_permissions: Option<AdminPermissionsSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) groups: Option<Vec<GroupConfigEntry>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "feishuApps"
    )]
    pub(super) feishu_apps: Option<HashMap<String, FeishuAppConfig>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "wechatAccounts"
    )]
    pub(super) wechat_accounts: Option<HashMap<String, WechatAccountConfig>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "telegramBots"
    )]
    pub(super) telegram_bots: Option<Vec<TelegramBotConfig>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "llmConfigs"
    )]
    pub(super) llm_configs: Option<Vec<LlmConfig>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "activeLlmConfigId"
    )]
    pub(super) active_llm_config_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "activeQuickLlmConfigId"
    )]
    pub(super) active_quick_llm_config_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "activeCognitiveLlmConfigId"
    )]
    pub(super) active_cognitive_llm_config_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "thinkingEnabled"
    )]
    pub(super) thinking_enabled: Option<bool>,
    /// Pre-process stage 1: when enabled, the engine deterministically matches
    /// the incoming message to a skill (by triggers / when-to-use) and
    /// force-loads it before the main turn instead of only hinting.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "preTriggerSkill"
    )]
    pub(super) pre_trigger_skill: Option<bool>,
    /// Pre-process stage 2: when enabled, relevant cognitive-graph memory is
    /// retrieved for the incoming message and injected into the prompt before
    /// the main turn (independent of the env-level FTS memory pre-retrieval).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "preCognitive"
    )]
    pub(super) pre_cognitive: Option<bool>,
    /// After-process stage: when enabled, the conversation is proactively
    /// summarized/compacted (Claude-Code-style) after each turn so the context
    /// stays optimized and the model keeps understanding the whole conversation.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "afterProcess"
    )]
    pub(super) after_process: Option<bool>,
    /// Curated-memory stage: when enabled, (a) history dropped by compaction is
    /// consolidated into curated `memory/*.md` files, and (b) each request runs
    /// a hybrid FTS5/vector search over the curated memories and injects the
    /// relevant ones into the prompt (Claude-Code-style auto-memory).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "memoryRecall"
    )]
    pub(super) memory_recall: Option<bool>,
    /// Autonomous MCP dispatcher: when enabled, ready tasks on dispatch sources
    /// (e.g. the Kanban board) are auto-run by persona worker agents.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "dispatchEnabled"
    )]
    pub(super) dispatch_enabled: Option<bool>,
    /// User-set default flow handlers (open link / media / search / note) plus
    /// the per-widget disable list. Edited from Plugins → Widget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) defaults: Option<DefaultsConfig>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "embeddingConfig"
    )]
    pub(super) embedding_config: Option<EmbeddingConfig>,
    /// Whisper's own settings now (`sen-whisper` runtime) — kept as raw JSON
    /// purely so it round-trips untouched through every other save, until the
    /// runtime imports it on its first start and the daemon stops carrying it
    /// at all. See `crate::gateway::group_manager::llm`'s "Legacy engine
    /// settings" note.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "whisperConfig"
    )]
    pub(super) whisper_config: Option<serde_json::Value>,
    /// TTS's own settings now (`sen-tts` runtime) — same raw passthrough.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "ttsConfig")]
    pub(super) tts_config: Option<serde_json::Value>,
    /// OCR's own settings now (`sen-ocr` runtime) — same raw passthrough.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "ocrConfig")]
    pub(super) ocr_config: Option<serde_json::Value>,
    /// Typed decisions. `backend`/`local`/`online` are `sen-sysone`'s own
    /// sub-keys now (kept as raw JSON so they round-trip untouched); `gate`/
    /// `skills` are the daemon's — see `crate::decision::settings`.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "decisionConfig")]
    pub(super) decision_config: Option<serde_json::Value>,
    /// JEV v2.2 control plane (`crate::control_plane::ControlPlaneSettings`).
    /// Fully daemon-owned — unlike `decisionConfig`, no runtime shares this
    /// key, so it round-trips as a typed struct.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "controlPlane")]
    pub(super) control_plane: Option<crate::control_plane::ControlPlaneSettings>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "cognitiveConfig"
    )]
    pub(super) cognitive_config: Option<PersistedCognitiveConfig>,
}

/// Settings → Cognitive UI form. Maps 1:1 onto [`crate::config::CognitiveConfig`]
/// at boot via `apply_persisted_overrides`. All fields optional so older
/// config files keep working (missing fields fall back to env / defaults).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedCognitiveConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "maxConcurrent"
    )]
    pub max_concurrent: Option<usize>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "maxOutputChars"
    )]
    pub max_output_chars: Option<usize>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reflectMinChars"
    )]
    pub reflect_min_chars: Option<usize>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reflectMaxChars"
    )]
    pub reflect_max_chars: Option<usize>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reflectCooldownMs"
    )]
    pub reflect_cooldown_ms: Option<u64>,
    /// Session-window idle timeout (ms) before a buffered conversation
    /// window is flushed to cognify. See `CognitiveConfig`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "reflectWindowIdleMs"
    )]
    pub reflect_window_idle_ms: Option<u64>,
    /// Toggle for `MemoryConfig.cognitive_reflection` — auto-cognify
    /// every user message. Off = manual CogAdd only.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "autoReflection"
    )]
    pub auto_reflection: Option<bool>,
    /// Cadence (hours) for the periodic maintenance sweep that runs
    /// `cleanup_junk` + `merge_duplicate_entities`. `0` disables it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "maintenanceIntervalHours"
    )]
    pub maintenance_interval_hours: Option<u64>,
}

/// Fields that can be updated on a [`GroupBinding`].
/// All fields are optional; `None` means "keep existing value".
#[derive(Debug, Clone, Default)]
pub struct GroupBindingUpdate {
    pub folder: Option<String>,
    pub name: Option<String>,
    pub channel: Option<String>,
    pub group_type: Option<String>,
    pub requires_trigger: Option<bool>,
    pub allowed_tools: Option<Option<Vec<String>>>,
    pub allowed_paths: Option<Option<Vec<String>>>,
    pub allowed_work_dirs: Option<Option<Vec<String>>>,
    pub bot_token: Option<Option<String>>,
    pub max_messages: Option<Option<u32>>,
    pub llm_config_id: Option<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct AdminPermissions {
    pub skip_main_agent_permissions: bool,
    pub skip_all_agents_permissions: bool,
}

pub struct LlmConfigResult {
    pub configs: Vec<LlmConfig>,
    pub active_id: Option<String>,
    pub active_quick_id: Option<String>,
    pub active_cognitive_id: Option<String>,
}


#[cfg(test)]
mod defaults_tests {
    use super::DefaultsConfig;

    #[test]
    fn empty_defaults_render_no_prompt_block() {
        // Untouched installs must keep a byte-identical system prompt.
        assert_eq!(DefaultsConfig::default().render_prompt_block(), None);
    }

    #[test]
    fn effective_fallbacks_match_today() {
        let d = DefaultsConfig::default();
        assert_eq!(d.effective_open_link(), "system-browser");
        assert_eq!(d.effective_media(), "inline-widget");
        assert_eq!(d.effective_search(), "browser");
        assert_eq!(d.effective_search_engine(), "google");
        assert_eq!(d.effective_note(), "space-notes");
    }

    #[test]
    fn prompt_block_covers_configured_flows() {
        let d = DefaultsConfig {
            search: Some("search-app".into()),
            note: Some("wiki".into()),
            media: Some("inline-widget".into()),
            open_link: Some("mini-browser".into()),
            ..Default::default()
        };
        let block = d.render_prompt_block().unwrap();
        assert!(block.starts_with("## User defaults"));
        assert!(block.contains("mcp__search-mcp__search_query"), "{block}");
        assert!(block.contains("wiki_write"), "{block}");
        assert!(block.contains("emit_widget"), "{block}");
        assert!(block.contains("mini-browser-mcp"), "{block}");
    }

    #[test]
    fn prompt_block_engine_only_when_browser_search() {
        let d = DefaultsConfig {
            search_engine: Some("bing".into()),
            ..Default::default()
        };
        let block = d.render_prompt_block().unwrap();
        assert!(block.contains("bing"), "{block}");
        // But picking the search-app hides the engine line (engine is a
        // browser_search knob).
        let d2 = DefaultsConfig {
            search: Some("search-app".into()),
            search_engine: Some("bing".into()),
            ..Default::default()
        };
        let block2 = d2.render_prompt_block().unwrap();
        assert!(!block2.contains("bing"), "{block2}");
    }

    #[test]
    fn defaults_round_trip_serde() {
        let d = DefaultsConfig {
            open_link: Some("mini-browser".into()),
            disabled_widgets: Some(vec!["clock".into(), "crm.pipeline".into()]),
            ..Default::default()
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("openLink"));
        assert!(json.contains("disabledWidgets"));
        let back: DefaultsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }
}

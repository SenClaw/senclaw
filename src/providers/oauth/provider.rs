//! Static registry of the OAuth providers SenClaw can sign in to.
//!
//! Each entry describes a *subscription* login (Claude Code, OpenAI Codex,
//! Google Antigravity) rather than a metered API key. The client ids below are
//! the ones the vendors' own desktop clients ship publicly; they are public
//! clients in the OAuth sense, so the flow is protected by PKCE, not by a
//! secret (Google is the exception — its "installed app" profile mandates a
//! client_secret that is likewise shipped in the clear).
//!
//! Note the `risk_notice` on every entry. Using a subscription credential from
//! a third-party client is outside each vendor's terms of service, and all
//! three actively detect it. The UI surfaces this string so the choice is
//! made with open eyes.
//!
//! Google's two clients (Antigravity, Gemini CLI) can be swapped without
//! editing this file: `SENCLAW_{ANTIGRAVITY,GEMINI_CLI}_OAUTH_CLIENT_{ID,SECRET}`
//! override the built-in values — from the daemon's environment at runtime
//! ([`OauthProviderDef::client_id`]) or from the build environment at compile
//! time ([`built_in`], which release CI feeds from repository secrets). An
//! empty value counts as unset.

/// How a provider wants the refresh-token request encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyEncoding {
    /// `application/json` body (Anthropic).
    Json,
    /// `application/x-www-form-urlencoded` body (OpenAI, Google).
    Form,
}

/// Which grant the provider implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowKind {
    /// RFC 7636: browser redirect back to a loopback listener.
    AuthCodePkce,
    /// RFC 8628: the user types a short code on the provider's site while we
    /// poll the token endpoint. No local listener, so it works over SSH and
    /// on machines where the daemon can't open a browser.
    DeviceCode,
}

/// Where the authorization code comes back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackPort {
    /// Bind any free port; the provider accepts a loopback redirect on any port
    /// (RFC 8252 §7.3).
    Ephemeral,
    /// The provider registered exactly one loopback port and rejects others.
    Fixed(u16),
}

/// The client credential compiled in: the build environment's value when it
/// is set, else the one written here. Empty counts as unset — CI expands a
/// repository secret that does not exist to an empty string, and that must
/// keep the built-in value rather than blank it.
const fn built_in(from_build: Option<&'static str>, default: &'static str) -> &'static str {
    match from_build {
        Some(v) if !v.is_empty() => v,
        _ => default,
    }
}

/// One signable provider.
#[derive(Debug, Clone)]
pub struct OauthProviderDef {
    /// Stable key used in config, the REST API and the store.
    pub id: &'static str,
    pub display_name: &'static str,
    /// Short reason this provider is risky, shown verbatim in the UI.
    pub risk_notice: &'static str,
    /// Brand colour (hex) for the UI badge.
    pub brand_color: &'static str,
    /// One or two characters used as the badge monogram. Deliberately a
    /// monogram rather than a traced vendor logo: an inaccurate redraw of
    /// someone's trademark is worse than a clean initial.
    pub brand_mark: &'static str,

    pub client_id: &'static str,
    /// Only Google's installed-app profile requires one.
    pub client_secret: Option<&'static str>,
    /// Prefix of the environment variables that override the built-in client
    /// at runtime: `<prefix>_CLIENT_ID` and `<prefix>_CLIENT_SECRET`. Read the
    /// client through [`Self::client_id`] / [`Self::client_secret`], never the
    /// fields directly, so an override is honoured everywhere.
    pub env_prefix: Option<&'static str>,
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    pub scopes: &'static [&'static str],
    /// Extra query parameters appended to the authorize URL.
    pub extra_authorize_params: &'static [(&'static str, &'static str)],

    pub flow: FlowKind,
    /// Device-authorization endpoint. Required when [`Self::flow`] is
    /// [`FlowKind::DeviceCode`], ignored otherwise.
    pub device_code_url: Option<&'static str>,

    /// Loopback settings — only meaningful for [`FlowKind::AuthCodePkce`].
    pub callback_port: CallbackPort,
    pub callback_path: &'static str,

    /// Encoding for both the code exchange and the refresh call — the three
    /// providers happen to use the same one for each.
    pub body_encoding: BodyEncoding,
    /// Whether `client_secret` goes in the token/refresh body.
    pub sends_client_secret: bool,
    /// Whether the refresh body must repeat the original scope. OpenAI rejects
    /// the refresh without it; the others ignore it.
    pub refresh_includes_scope: bool,
    /// Refresh this many seconds before `expires_at`. Vendors differ wildly:
    /// Anthropic issues short-lived tokens, OpenAI's last days.
    pub refresh_lead_secs: i64,

    /// The `adapt` value an `LlmConfig` gets when bound to this provider.
    pub adapt: &'static str,
    /// Base URL the adapter talks to.
    pub base_url: &'static str,
    /// Suggested models — `(id, display name)`. Used to pre-fill the UI picker;
    /// the user can always type another id.
    pub default_models: &'static [(&'static str, &'static str)],
    /// Sensible default for a new config's max output tokens.
    pub default_max_tokens: u32,
    pub default_context_length: u32,
}

impl OauthProviderDef {
    /// The client id to send: `<env_prefix>_CLIENT_ID` from the daemon's
    /// environment when set, else the built-in one.
    pub fn client_id(&self) -> String {
        self.env_override("CLIENT_ID").unwrap_or_else(|| self.client_id.to_string())
    }

    /// The client secret to send, overridable like [`Self::client_id`].
    pub fn client_secret(&self) -> Option<String> {
        self.env_override("CLIENT_SECRET").or_else(|| self.client_secret.map(str::to_string))
    }

    fn env_override(&self, suffix: &str) -> Option<String> {
        let prefix = self.env_prefix?;
        std::env::var(format!("{prefix}_{suffix}")).ok().filter(|v| !v.trim().is_empty())
    }

    /// Space-joined scope string for the authorize/refresh requests.
    pub fn scope_string(&self) -> String {
        self.scopes.join(" ")
    }

    /// The loopback redirect this provider must be handed. Kept in one place
    /// because the exchange call has to repeat it byte-for-byte.
    pub fn redirect_uri(&self, port: u16) -> String {
        format!("http://localhost:{port}{}", self.callback_path)
    }
}

const CLAUDE: OauthProviderDef = OauthProviderDef {
    id: "claude",
    display_name: "Claude Code",
    risk_notice: "Uses your Claude subscription outside Anthropic's own clients. \
                  This is against Anthropic's terms of service and can get the account suspended.",
    brand_color: "#D97757",
    brand_mark: "C",
    client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
    client_secret: None,
    env_prefix: None,
    authorize_url: "https://claude.ai/oauth/authorize",
    token_url: "https://api.anthropic.com/v1/oauth/token",
    scopes: &["org:create_api_key", "user:profile", "user:inference"],
    // Anthropic's authorize endpoint needs `code=true` to return the code on
    // the loopback redirect instead of rendering a paste-me page.
    extra_authorize_params: &[("code", "true")],
    flow: FlowKind::AuthCodePkce,
    device_code_url: None,
    callback_port: CallbackPort::Ephemeral,
    callback_path: "/callback",
    body_encoding: BodyEncoding::Json,
    sends_client_secret: false,
    refresh_includes_scope: false,
    refresh_lead_secs: 4 * 60 * 60,
    adapt: "anthropic",
    base_url: "https://api.anthropic.com",
    default_models: &[
        ("claude-opus-5", "Claude Opus 5"),
        ("claude-fable-5", "Claude Fable 5"),
        ("claude-sonnet-5", "Claude Sonnet 5"),
        ("claude-haiku-4-5-20251001", "Claude 4.5 Haiku"),
    ],
    default_max_tokens: 8192,
    default_context_length: 200_000,
};

const CODEX: OauthProviderDef = OauthProviderDef {
    id: "codex",
    display_name: "OpenAI Codex",
    risk_notice: "Uses your ChatGPT subscription outside OpenAI's own clients. \
                  This is against OpenAI's terms of service and can get the account suspended.",
    brand_color: "#10A37F",
    brand_mark: "OA",
    client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
    client_secret: None,
    env_prefix: None,
    authorize_url: "https://auth.openai.com/oauth/authorize",
    token_url: "https://auth.openai.com/oauth/token",
    scopes: &["openid", "profile", "email", "offline_access"],
    extra_authorize_params: &[
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
    ],
    flow: FlowKind::AuthCodePkce,
    device_code_url: None,
    // OpenAI registered exactly one loopback redirect for this client.
    callback_port: CallbackPort::Fixed(1455),
    callback_path: "/auth/callback",
    body_encoding: BodyEncoding::Form,
    sends_client_secret: false,
    refresh_includes_scope: true,
    refresh_lead_secs: 5 * 24 * 60 * 60,
    adapt: "codex",
    base_url: "https://chatgpt.com/backend-api/codex",
    default_models: &[
        ("gpt-5.6-sol", "GPT 5.6 Sol"),
        ("gpt-5.6-terra", "GPT 5.6 Terra"),
        ("gpt-5.6-luna", "GPT 5.6 Luna"),
        ("gpt-5.5", "GPT 5.5"),
        ("gpt-5.4", "GPT 5.4"),
        ("gpt-5.4-mini", "GPT 5.4 Mini"),
        ("gpt-5.3-codex-spark", "GPT 5.3 Codex Spark"),
    ],
    default_max_tokens: 16384,
    default_context_length: 272_000,
};

const ANTIGRAVITY: OauthProviderDef = OauthProviderDef {
    id: "antigravity",
    display_name: "Antigravity",
    risk_notice: "Uses your Google Antigravity entitlement through an unpublished internal API. \
                  This is against Google's terms of service and can get the account suspended.",
    brand_color: "#F59E0B",
    brand_mark: "AG",
    client_id: built_in(option_env!("SENCLAW_ANTIGRAVITY_OAUTH_CLIENT_ID"), "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"),
    // Google's installed-app profile requires the secret on token calls even
    // though it ships inside the desktop client. It is not a secret in practice.
    // Both can be overridden from the environment (see `env_prefix`).
    client_secret: Some(built_in(option_env!("SENCLAW_ANTIGRAVITY_OAUTH_CLIENT_SECRET"), "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")),
    env_prefix: Some("SENCLAW_ANTIGRAVITY_OAUTH"),
    authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    scopes: &[
        "https://www.googleapis.com/auth/cloud-platform",
        "https://www.googleapis.com/auth/userinfo.email",
        "https://www.googleapis.com/auth/userinfo.profile",
        "https://www.googleapis.com/auth/cclog",
        "https://www.googleapis.com/auth/experimentsandconfigs",
    ],
    // Without these Google returns an access token but no refresh token, and
    // the account silently dies an hour later.
    extra_authorize_params: &[("access_type", "offline"), ("prompt", "consent")],
    flow: FlowKind::AuthCodePkce,
    device_code_url: None,
    callback_port: CallbackPort::Ephemeral,
    callback_path: "/callback",
    body_encoding: BodyEncoding::Form,
    sends_client_secret: true,
    refresh_includes_scope: false,
    refresh_lead_secs: 5 * 60,
    adapt: "antigravity",
    // Completions go to the `daily-` host, *not* prod. The two split duties:
    // discovery (loadCodeAssist / onboardUser / fetchAvailableModels) only
    // works on prod, and prod validates the project as a real Cloud consumer —
    // which is why a request aimed there with a generated project id comes back
    // `CONSUMER_INVALID`. Gemini CLI is the mirror image and uses prod for both.
    base_url: "https://daily-cloudcode-pa.googleapis.com",
    default_models: &[
        ("gemini-3.6-flash-high", "Gemini 3.6 Flash (High)"),
        ("gemini-3.6-flash-medium", "Gemini 3.6 Flash (Medium)"),
        ("gemini-3.6-flash-low", "Gemini 3.6 Flash (Low)"),
        ("gemini-3.5-flash-high", "Gemini 3.5 Flash (High)"),
        ("gemini-3.5-flash-low", "Gemini 3.5 Flash (Medium)"),
        ("gemini-3.5-flash-extra-low", "Gemini 3.5 Flash (Low)"),
        ("gemini-pro-agent", "Gemini 3.1 Pro (High)"),
        ("gemini-3.1-pro-low", "Gemini 3.1 Pro (Low)"),
        ("gemini-3-flash", "Gemini 3 Flash"),
        ("claude-sonnet-4-6", "Claude Sonnet 4.6 (Thinking)"),
        ("claude-opus-4-6-thinking", "Claude Opus 4.6 (Thinking)"),
        ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
    ],
    default_max_tokens: 8192,
    default_context_length: 1_000_000,
};

const GITHUB_COPILOT: OauthProviderDef = OauthProviderDef {
    id: "github-copilot",
    display_name: "GitHub Copilot",
    risk_notice: "Uses your Copilot subscription outside GitHub's own editors, which its terms \
                  do not permit. The account can be suspended.",
    brand_color: "#8B5CF6",
    brand_mark: "GH",
    client_id: "Iv1.b507a08c87ecfe98",
    client_secret: None,
    env_prefix: None,
    // Device flow has no browser redirect; the user opens this page themselves.
    authorize_url: "https://github.com/login/device",
    token_url: "https://github.com/login/oauth/access_token",
    scopes: &["read:user"],
    extra_authorize_params: &[],
    flow: FlowKind::DeviceCode,
    device_code_url: Some("https://github.com/login/device/code"),
    callback_port: CallbackPort::Ephemeral,
    callback_path: "/callback",
    body_encoding: BodyEncoding::Form,
    sends_client_secret: false,
    refresh_includes_scope: false,
    refresh_lead_secs: 5 * 60,
    adapt: "openai",
    base_url: "https://api.githubcopilot.com",
    default_models: &[
        ("gpt-5.4", "GPT-5.4"),
        ("gpt-5.4-mini", "GPT-5.4 Mini"),
        ("gpt-5.3-codex", "GPT-5.3 Codex"),
        ("gpt-5.2", "GPT-5.2"),
        ("gpt-5.2-codex", "GPT-5.2 Codex"),
        ("claude-opus-4.7", "Claude Opus 4.7"),
        ("claude-opus-4.6", "Claude Opus 4.6"),
        ("claude-sonnet-4.6", "Claude Sonnet 4.6"),
        ("claude-sonnet-4.5", "Claude Sonnet 4.5"),
        ("claude-haiku-4.5", "Claude Haiku 4.5"),
        ("gemini-3.1-pro-preview", "Gemini 3.1 Pro"),
        ("gemini-3-flash-preview", "Gemini 3 Flash"),
        ("gemini-2.5-pro", "Gemini 2.5 Pro"),
        ("grok-code-fast-1", "Grok Code Fast 1"),
    ],
    default_max_tokens: 8192,
    default_context_length: 128_000,
};

const QWEN: OauthProviderDef = OauthProviderDef {
    id: "qwen",
    display_name: "Qwen Code",
    risk_notice: "Uses your Qwen account through the Qwen Code CLI's client credentials rather \
                  than a published API key.",
    brand_color: "#615CED",
    brand_mark: "Q",
    client_id: "f0304373b74a44d2b584a3fb70ca9e56",
    client_secret: None,
    env_prefix: None,
    authorize_url: "https://chat.qwen.ai/authorize",
    token_url: "https://chat.qwen.ai/api/v1/oauth2/token",
    scopes: &["openid", "profile", "email", "model.completion"],
    extra_authorize_params: &[],
    flow: FlowKind::DeviceCode,
    device_code_url: Some("https://chat.qwen.ai/api/v1/oauth2/device/code"),
    callback_port: CallbackPort::Ephemeral,
    callback_path: "/callback",
    body_encoding: BodyEncoding::Form,
    sends_client_secret: false,
    refresh_includes_scope: false,
    refresh_lead_secs: 5 * 60,
    adapt: "openai",
    base_url: "https://portal.qwen.ai/v1",
    default_models: &[
        ("qwen3-coder-plus", "Qwen3 Coder Plus"),
        ("qwen3-coder-flash", "Qwen3 Coder Flash"),
        ("coder-model", "Qwen3.6 Coder Model"),
        ("vision-model", "Qwen3 Vision Model"),
    ],
    default_max_tokens: 8192,
    default_context_length: 256_000,
};

const KIMI: OauthProviderDef = OauthProviderDef {
    id: "kimi",
    display_name: "Kimi for Coding",
    risk_notice: "Uses your Kimi coding plan through the Kimi CLI's client credentials rather \
                  than a published API key.",
    brand_color: "#00D2FF",
    brand_mark: "K",
    client_id: "17e5f671-d194-4dfb-9706-5516cb48c098",
    client_secret: None,
    env_prefix: None,
    authorize_url: "https://auth.kimi.com/device",
    token_url: "https://auth.kimi.com/api/oauth/token",
    scopes: &[],
    extra_authorize_params: &[],
    flow: FlowKind::DeviceCode,
    device_code_url: Some("https://auth.kimi.com/api/oauth/device_authorization"),
    callback_port: CallbackPort::Ephemeral,
    callback_path: "/callback",
    body_encoding: BodyEncoding::Form,
    sends_client_secret: false,
    refresh_includes_scope: false,
    refresh_lead_secs: 5 * 60,
    // Kimi's coding endpoint speaks the Anthropic Messages protocol.
    adapt: "anthropic",
    base_url: "https://api.kimi.com/coding",
    default_models: &[
        ("kimi-k3", "Kimi K3"),
        ("k3", "Kimi K3 (Code)"),
        ("kimi-for-coding", "Kimi for Coding"),
        ("kimi-for-coding-highspeed", "Kimi for Coding Highspeed"),
        ("kimi-k2.7-code", "Kimi K2.7 Code"),
        ("kimi-k2.7-code-highspeed", "Kimi K2.7 Code Highspeed"),
        ("kimi-k2.6", "Kimi K2.6"),
        ("kimi-k2.5", "Kimi K2.5"),
        ("kimi-k2.5-thinking", "Kimi K2.5 Thinking"),
        ("kimi-latest", "Kimi Latest"),
    ],
    default_max_tokens: 8192,
    default_context_length: 256_000,
};

const GROK: OauthProviderDef = OauthProviderDef {
    id: "grok",
    display_name: "Grok CLI",
    risk_notice: "Uses your SuperGrok subscription outside xAI's own client, which its terms \
                  do not permit. The account can be suspended.",
    // xAI's mark is black-on-white; a near-black badge vanishes against the
    // dark UI, so the badge uses the light side of the brand instead.
    brand_color: "#9CA3AF",
    brand_mark: "X",
    client_id: "b1a00492-073a-47ea-816f-4c329264a828",
    client_secret: None,
    env_prefix: None,
    authorize_url: "https://accounts.x.ai/device",
    token_url: "https://auth.x.ai/oauth2/token",
    scopes: &[
        "openid",
        "profile",
        "email",
        "offline_access",
        "grok-cli:access",
        "api:access",
        "conversations:read",
        "conversations:write",
    ],
    extra_authorize_params: &[],
    flow: FlowKind::DeviceCode,
    device_code_url: Some("https://auth.x.ai/oauth2/device/code"),
    callback_port: CallbackPort::Ephemeral,
    callback_path: "/callback",
    body_encoding: BodyEncoding::Form,
    sends_client_secret: false,
    refresh_includes_scope: false,
    refresh_lead_secs: 5 * 60,
    // Grok's CLI proxy speaks the OpenAI Responses API, same as Codex.
    adapt: "codex",
    base_url: "https://cli-chat-proxy.grok.com",
    default_models: &[
        ("grok-4.5", "Grok 4.5"),
        ("grok-4.5-high", "Grok 4.5 (High)"),
        ("grok-4.5-medium", "Grok 4.5 (Medium)"),
        ("grok-4.5-low", "Grok 4.5 (Low)"),
    ],
    default_max_tokens: 8192,
    default_context_length: 256_000,
};

const IFLOW: OauthProviderDef = OauthProviderDef {
    id: "iflow",
    display_name: "iFlow",
    risk_notice: "Uses the iFlow CLI's client credentials rather than a published API key.",
    brand_color: "#0EA5E9",
    brand_mark: "iF",
    client_id: "10009311001",
    client_secret: Some("4Z3YjXycVsQvyGF1etiNlIBB4RsqSDtW"),
    env_prefix: None,
    authorize_url: "https://iflow.cn/oauth",
    token_url: "https://iflow.cn/oauth/token",
    scopes: &[],
    extra_authorize_params: &[],
    flow: FlowKind::AuthCodePkce,
    device_code_url: None,
    callback_port: CallbackPort::Ephemeral,
    callback_path: "/callback",
    body_encoding: BodyEncoding::Form,
    sends_client_secret: true,
    refresh_includes_scope: false,
    refresh_lead_secs: 5 * 60,
    adapt: "openai",
    base_url: "https://apis.iflow.cn/v1",
    default_models: &[
        ("qwen3-coder-plus", "Qwen3 Coder Plus"),
        ("qwen3-max", "Qwen3 Max"),
        ("qwen3-vl-plus", "Qwen3 VL Plus"),
        ("qwen3-max-preview", "Qwen3 Max Preview"),
        ("qwen3-235b", "Qwen3 235B A22B"),
        ("qwen3-235b-a22b-instruct", "Qwen3 235B A22B Instruct"),
        ("qwen3-235b-a22b-thinking-2507", "Qwen3 235B A22B Thinking"),
        ("qwen3-32b", "Qwen3 32B"),
        ("kimi-k2", "Kimi K2"),
        ("deepseek-v3.2", "DeepSeek V3.2 Exp"),
        ("deepseek-v3.1", "DeepSeek V3.1 Terminus"),
        ("deepseek-v3", "DeepSeek V3 671B"),
        ("deepseek-r1", "DeepSeek R1"),
        ("glm-4.7", "GLM 4.7"),
        ("iflow-rome-30ba3b", "iFlow ROME"),
    ],
    default_max_tokens: 8192,
    default_context_length: 256_000,
};

const GEMINI_CLI: OauthProviderDef = OauthProviderDef {
    id: "gemini-cli",
    display_name: "Gemini CLI",
    risk_notice: "Uses the Gemini CLI's client credentials against Google's Code Assist API \
                  rather than a published API key. Google's terms do not permit this from a \
                  third-party client.",
    brand_color: "#4285F4",
    brand_mark: "GC",
    // Google's published Gemini CLI desktop client; overridable from the environment.
    client_id: built_in(option_env!("SENCLAW_GEMINI_CLI_OAUTH_CLIENT_ID"), "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"),
    client_secret: Some(built_in(option_env!("SENCLAW_GEMINI_CLI_OAUTH_CLIENT_SECRET"), "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")),
    env_prefix: Some("SENCLAW_GEMINI_CLI_OAUTH"),
    authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    scopes: &[
        "https://www.googleapis.com/auth/cloud-platform",
        "https://www.googleapis.com/auth/userinfo.email",
        "https://www.googleapis.com/auth/userinfo.profile",
    ],
    extra_authorize_params: &[("access_type", "offline"), ("prompt", "consent")],
    flow: FlowKind::AuthCodePkce,
    device_code_url: None,
    callback_port: CallbackPort::Ephemeral,
    callback_path: "/callback",
    body_encoding: BodyEncoding::Form,
    sends_client_secret: true,
    refresh_includes_scope: false,
    refresh_lead_secs: 5 * 60,
    // Same Code Assist surface as Antigravity, so it reuses that adapter.
    adapt: "antigravity",
    base_url: "https://cloudcode-pa.googleapis.com",
    default_models: &[
        ("gemini-3.1-pro-preview", "Gemini 3.1 Pro Preview"),
        ("gemini-3-pro-preview", "Gemini 3 Pro Preview"),
        ("gemini-3-flash-preview", "Gemini 3 Flash Preview"),
        ("gemini-3.1-flash-lite-preview", "Gemini 3.1 Flash Lite Preview"),
        ("gemini-2.5-pro", "Gemini 2.5 Pro"),
        ("gemini-2.5-flash", "Gemini 2.5 Flash"),
        ("gemini-2.5-flash-lite", "Gemini 2.5 Flash Lite"),
    ],
    default_max_tokens: 8192,
    default_context_length: 1_000_000,
};

/// Google's userinfo endpoint — used once after sign-in to label the account.
pub const GOOGLE_USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v1/userinfo";

/// Code Assist project discovery, required before Antigravity will serve
/// completions for a fresh account.
pub const ANTIGRAVITY_LOAD_CODE_ASSIST_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
pub const ANTIGRAVITY_ONBOARD_USER_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:onboardUser";

static ALL: &[OauthProviderDef] = &[
    CLAUDE,
    CODEX,
    ANTIGRAVITY,
    GITHUB_COPILOT,
    QWEN,
    KIMI,
    GROK,
    IFLOW,
    GEMINI_CLI,
];

/// Every provider the UI can offer.
pub fn all() -> &'static [OauthProviderDef] {
    ALL
}

/// Look up a provider by its stable id.
pub fn get(id: &str) -> Option<&'static OauthProviderDef> {
    ALL.iter().find(|p| p.id == id)
}

/// True when `adapt` is served by an OAuth-only provider — i.e. there is no
/// API-key form of it, so the LLM layer must resolve a token.
pub fn adapt_is_oauth_only(adapt: &str) -> bool {
    matches!(adapt, "codex" | "antigravity")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_is_addressable_by_id() {
        for p in all() {
            assert!(get(p.id).is_some(), "{} not resolvable", p.id);
        }
        assert!(get("nope").is_none());
    }

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<_> = all().iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate provider id");
    }

    #[test]
    fn redirect_uri_uses_declared_path() {
        assert_eq!(
            get("claude").unwrap().redirect_uri(1234),
            "http://localhost:1234/callback"
        );
        assert_eq!(
            get("codex").unwrap().redirect_uri(1455),
            "http://localhost:1455/auth/callback"
        );
    }

    #[test]
    fn codex_pins_its_registered_loopback_port() {
        assert_eq!(
            get("codex").unwrap().callback_port,
            CallbackPort::Fixed(1455)
        );
    }

    #[test]
    fn google_requests_offline_access_so_a_refresh_token_is_issued() {
        let ag = get("antigravity").unwrap();
        assert!(
            ag.extra_authorize_params
                .contains(&("access_type", "offline"))
        );
        assert!(ag.extra_authorize_params.contains(&("prompt", "consent")));
        assert!(ag.sends_client_secret);
        assert!(ag.client_secret.is_some());
    }

    #[test]
    fn a_provider_sends_a_client_secret_only_when_it_has_one() {
        for p in all() {
            if p.sends_client_secret {
                assert!(p.client_secret.is_some(), "{} has none to send", p.id);
            }
        }
    }

    #[test]
    fn a_client_secret_appears_only_where_the_vendor_mandates_one() {
        // Google's installed-app profile and iFlow's token endpoint both
        // require a secret that ships in the clear. Every other provider is a
        // true public client and must rely on PKCE alone — a stray secret
        // there would mean we copied config from the wrong place.
        let expect_secret = ["antigravity", "iflow", "gemini-cli"];
        for p in all() {
            assert_eq!(
                p.client_secret.is_some(),
                expect_secret.contains(&p.id),
                "{}",
                p.id
            );
        }
    }

    #[test]
    fn a_non_empty_build_value_replaces_the_built_in_one() {
        // CI expands a missing repository secret to "", which must keep the default.
        assert_eq!(built_in(Some("from-ci"), "default"), "from-ci");
        assert_eq!(built_in(Some(""), "default"), "default");
        assert_eq!(built_in(None, "default"), "default");
    }

    #[test]
    fn the_environment_overrides_the_built_in_client_at_runtime() {
        // A prefix of its own, so no other test ever reads these variables.
        let prefix = "SENCLAW_TEST_OAUTH_OVERRIDE";
        let def = OauthProviderDef { env_prefix: Some(prefix), ..ANTIGRAVITY };
        let (id_var, secret_var) = (format!("{prefix}_CLIENT_ID"), format!("{prefix}_CLIENT_SECRET"));

        assert_eq!(def.client_id(), ANTIGRAVITY.client_id);
        assert_eq!(def.client_secret().as_deref(), ANTIGRAVITY.client_secret);

        std::env::set_var(&id_var, "override-id");
        std::env::set_var(&secret_var, "override-secret");
        assert_eq!(def.client_id(), "override-id");
        assert_eq!(def.client_secret().as_deref(), Some("override-secret"));

        // Blank is unset, not an empty credential.
        std::env::set_var(&id_var, "  ");
        assert_eq!(def.client_id(), ANTIGRAVITY.client_id);

        std::env::remove_var(&id_var);
        std::env::remove_var(&secret_var);
    }

    #[test]
    fn only_google_clients_are_overridable() {
        for p in all() {
            assert_eq!(p.env_prefix.is_some(), matches!(p.id, "antigravity" | "gemini-cli"), "{}", p.id);
        }
        assert_eq!(get("antigravity").unwrap().env_prefix, Some("SENCLAW_ANTIGRAVITY_OAUTH"));
        assert_eq!(get("gemini-cli").unwrap().env_prefix, Some("SENCLAW_GEMINI_CLI_OAUTH"));
    }

    #[test]
    fn device_flow_providers_declare_a_device_endpoint() {
        for p in all() {
            match p.flow {
                FlowKind::DeviceCode => assert!(
                    p.device_code_url.is_some(),
                    "{} has no device-code endpoint",
                    p.id
                ),
                FlowKind::AuthCodePkce => assert!(
                    p.device_code_url.is_none(),
                    "{} declares a device endpoint it never uses",
                    p.id
                ),
            }
        }
    }

    #[test]
    fn every_adapt_has_an_implementation_or_an_explicit_guard() {
        // `adapt` drives the match in query_llm; a value with no arm there
        // would silently fall through to the OpenAI adapter.
        for p in all() {
            assert!(
                matches!(p.adapt, "openai" | "anthropic" | "codex" | "antigravity"),
                "{} uses unrouted adapt `{}`",
                p.id,
                p.adapt
            );
        }
    }

    #[test]
    fn base_urls_are_https_without_a_trailing_slash() {
        for p in all() {
            assert!(p.base_url.starts_with("https://"), "{}", p.id);
            assert!(!p.base_url.ends_with('/'), "{} trailing slash", p.id);
        }
    }

    #[test]
    fn scope_string_is_space_joined() {
        assert_eq!(
            get("claude").unwrap().scope_string(),
            "org:create_api_key user:profile user:inference"
        );
    }

    #[test]
    fn every_provider_states_its_risk() {
        for p in all() {
            assert!(!p.risk_notice.is_empty(), "{} has no risk notice", p.id);
        }
    }

    #[test]
    fn oauth_only_adapts_have_no_api_key_equivalent() {
        assert!(adapt_is_oauth_only("codex"));
        assert!(adapt_is_oauth_only("antigravity"));
        // Anthropic is reachable with a plain API key too.
        assert!(!adapt_is_oauth_only("anthropic"));
        assert!(!adapt_is_oauth_only("openai"));
    }
}

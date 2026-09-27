//! Turning a stored account into the headers and URLs an LLM request needs.
//!
//! Kept separate from [`super::flow`] so the LLM layer can depend on request
//! decoration without pulling in the sign-in machinery.
//!
//! ## Client identity
//!
//! Requests default to a `senclaw/<version>` User-Agent, and Claude and Codex
//! are called that way: their endpoints serve a request that names us.
//!
//! Google's Code Assist does not. It answers 403 to anything but its own
//! client, with no documented third-party mode, so the Antigravity path sends
//! the IDE's identity strings instead. Those live in one clearly-named block
//! below rather than scattered inline, because it is a real trade-off and
//! should be easy to find, audit, and change.
//!
//! Not present, and deliberately: response-shaping fingerprints. No
//! `X-Stainless-*` SDK telemetry, no synthetic device/account/session ids, no
//! fabricated billing header, no decoy tool declarations. Those exist purely to
//! make third-party traffic statistically indistinguishable from first-party
//! traffic; sending the identity an endpoint demands in order to answer at all
//! is a different thing from manufacturing a disguise.

use super::provider::OauthProviderDef;
use super::store::OauthAccount;

/// Anthropic's beta flag that makes `/v1/messages` accept an OAuth bearer
/// token in place of `x-api-key`. Without it the endpoint 401s.
pub const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

/// Anthropic message-format version this codebase speaks.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// How SenClaw identifies itself upstream by default.
pub fn user_agent() -> String {
    format!("senclaw/{}", env!("CARGO_PKG_VERSION"))
}

// ---------------------------------------------------------------------------
// Vendor client identity
// ---------------------------------------------------------------------------
//
// Google's Code Assist endpoints are not a public API: they answer 403 unless
// the caller presents the identity of the vendor's own client. There is no
// documented third-party mode, so a request that names SenClaw is refused no
// matter how well formed it is.
//
// These constants therefore carry the Antigravity IDE's identity. That is a
// deliberate, user-authorised choice, not an oversight — it is what makes the
// provider usable at all, and it is exactly why the UI marks Antigravity with a
// terms-of-service warning. They are named and grouped here rather than inlined
// so the behaviour is visible in one place and easy to change.

/// IDE build the Antigravity client reports. Google gates newer models on it.
pub const ANTIGRAVITY_IDE_VERSION: &str = "1.23.2";

/// `User-Agent` the Code Assist completion endpoint expects.
pub fn antigravity_user_agent() -> String {
    let arch = if std::env::consts::ARCH == "aarch64" {
        "arm64"
    } else {
        "x64"
    };
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    format!("antigravity/ide/{ANTIGRAVITY_IDE_VERSION} {os}/{arch}")
}

/// `User-Agent` the Code Assist *discovery* endpoints expect. Different from
/// the completion one: those calls come from the Google API client library
/// rather than the IDE binary.
pub const CODE_ASSIST_DISCOVERY_USER_AGENT: &str = "google-api-nodejs-client/9.15.1";

/// `X-Goog-Api-Client` accompanying the discovery calls.
pub const CODE_ASSIST_DISCOVERY_API_CLIENT: &str = "google-cloud-sdk vscode_cloudshelleditor/0.1";

/// Headers to attach to an OAuth-authenticated LLM request.
///
/// Returned as pairs rather than a `HeaderMap` so callers can feed them
/// straight into `reqwest`'s builder without a conversion dance.
pub fn auth_headers(provider_id: &str, access_token: &str) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("Authorization", format!("Bearer {access_token}")),
        ("User-Agent", user_agent()),
    ];

    match provider_id {
        "claude" => {
            headers.push(("anthropic-version", ANTHROPIC_VERSION.to_string()));
            headers.push(("anthropic-beta", ANTHROPIC_OAUTH_BETA.to_string()));
        }
        "codex" => {
            // Identifies the calling client to the Codex backend. We send our
            // own name; see the module note about not impersonating the CLI.
            headers.push(("originator", "senclaw".to_string()));
        }
        "antigravity" => {
            // The completion endpoint receives exactly three headers from the
            // IDE: Content-Type (set by the JSON body), Authorization, and this
            // User-Agent. Anything extra — an `x-goog-api-client`, say — is
            // enough for Code Assist to answer 403.
            headers.retain(|(name, _)| !name.eq_ignore_ascii_case("User-Agent"));
            headers.push(("User-Agent", antigravity_user_agent()));
        }
        _ => {}
    }

    headers
}

/// Platform enum used by the Code Assist `clientMetadata` envelope. These are
/// the values Google's schema defines; the one we report is our actual host,
/// not a spoofed one.
fn platform_enum() -> u8 {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => 2,
        ("macos", _) => 1,
        ("linux", "aarch64") => 4,
        ("linux", _) => 3,
        ("windows", _) => 5,
        _ => 0,
    }
}

/// The `clientMetadata` object Code Assist requires on discovery calls.
///
/// `ideType`/`pluginType` are mandatory enums in Google's schema with no
/// "other" member, so there is no honest value to send; they are structural,
/// and the request is rejected outright without them.
pub fn code_assist_client_metadata() -> serde_json::Value {
    serde_json::json!({
        "ideType": 9,
        "platform": platform_enum(),
        "pluginType": 2,
    })
}

/// Project id discovered from Code Assist, cached on the account.
pub const ANTIGRAVITY_PROJECT_KEY: &str = "projectId";

/// Read the cached Code Assist project id, if discovery has already run.
pub fn cached_project_id(account: &OauthAccount) -> Option<String> {
    account
        .extra
        .get(ANTIGRAVITY_PROJECT_KEY)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// The endpoint an account's completions go to.
pub fn base_url(def: &OauthProviderDef) -> &'static str {
    def.base_url
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::oauth::provider;

    fn header<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn every_provider_gets_a_bearer_token() {
        for p in provider::all() {
            let h = auth_headers(p.id, "tok-123");
            assert_eq!(
                header(&h, "Authorization"),
                Some("Bearer tok-123"),
                "{}",
                p.id
            );
        }
    }

    #[test]
    fn claude_gets_the_oauth_beta_flag_and_version() {
        let h = auth_headers("claude", "t");
        assert_eq!(header(&h, "anthropic-version"), Some("2023-06-01"));
        assert_eq!(header(&h, "anthropic-beta"), Some("oauth-2025-04-20"));
    }

    #[test]
    fn claude_headers_do_not_claim_to_be_claude_code() {
        let h = auth_headers("claude", "t");
        let joined = format!("{h:?}").to_lowercase();
        // The identity markers other routers send to pass as the first-party
        // client. Their absence is the point of this module.
        assert!(!joined.contains("claude-cli"), "{joined}");
        assert!(!joined.contains("claude-code-2025"), "{joined}");
        assert!(!joined.contains("x-stainless"), "{joined}");
        assert!(!joined.contains("billing-header"), "{joined}");
        assert!(!joined.contains("x-app"), "{joined}");
    }

    #[test]
    fn codex_identifies_as_senclaw_not_the_codex_cli() {
        let h = auth_headers("codex", "t");
        assert_eq!(header(&h, "originator"), Some("senclaw"));
        let joined = format!("{h:?}").to_lowercase();
        assert!(!joined.contains("codex_cli_rs"), "{joined}");
    }

    #[test]
    fn user_agent_names_senclaw_and_its_version() {
        let ua = user_agent();
        assert!(ua.starts_with("senclaw/"), "{ua}");
        assert!(ua.len() > "senclaw/".len(), "{ua}");
        for p in provider::all() {
            // Antigravity is the exception: its endpoint only answers the IDE.
            if p.id == "antigravity" {
                continue;
            }
            let h = auth_headers(p.id, "t");
            assert_eq!(header(&h, "User-Agent"), Some(ua.as_str()), "{}", p.id);
        }
    }

    #[test]
    fn antigravity_sends_exactly_the_three_headers_code_assist_accepts() {
        // Content-Type comes from the JSON body, so the pair list carries two.
        // An extra header here is enough for the endpoint to answer 403.
        let h = auth_headers("antigravity", "tok");
        assert_eq!(h.len(), 2, "unexpected headers: {h:?}");
        assert_eq!(header(&h, "Authorization"), Some("Bearer tok"));

        let ua = header(&h, "User-Agent").expect("user agent");
        assert!(ua.starts_with("antigravity/ide/"), "{ua}");
        assert!(ua.contains(ANTIGRAVITY_IDE_VERSION), "{ua}");
        // Exactly one User-Agent — the default must have been replaced, not
        // appended alongside.
        assert_eq!(
            h.iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("User-Agent"))
                .count(),
            1
        );
    }

    #[test]
    fn the_antigravity_user_agent_names_the_running_platform() {
        let ua = antigravity_user_agent();
        let suffix = ua.rsplit(' ').next().expect("platform suffix");
        assert!(suffix.contains('/'), "{ua}");
        // Whatever host this builds on, the pair is a real one.
        assert!(
            ["darwin", "linux", "windows", "win32"]
                .iter()
                .any(|os| suffix.starts_with(os)),
            "{ua}"
        );
    }

    #[test]
    fn an_unknown_provider_still_gets_usable_auth() {
        let h = auth_headers("something-else", "t");
        assert_eq!(header(&h, "Authorization"), Some("Bearer t"));
        assert_eq!(h.len(), 2, "no provider-specific headers expected");
    }

    #[test]
    fn client_metadata_reports_a_defined_platform() {
        let meta = code_assist_client_metadata();
        let platform = meta["platform"].as_u64().unwrap();
        assert!(platform <= 5, "unknown platform enum {platform}");
        assert_eq!(meta["ideType"], 9);
        assert_eq!(meta["pluginType"], 2);
    }

    #[test]
    fn project_id_is_read_back_from_account_extras() {
        let mut acc = crate::providers::oauth::store::OauthAccount {
            id: "a".into(),
            provider: "antigravity".into(),
            label: "l".into(),
            access_token: "t".into(),
            refresh_token: None,
            expires_at: None,
            scope: None,
            email: None,
            extra: serde_json::Map::new(),
            created_at: 0,
            last_refresh_at: None,
            last_error: None,
        };
        assert_eq!(cached_project_id(&acc), None);

        acc.extra.insert(
            ANTIGRAVITY_PROJECT_KEY.into(),
            serde_json::json!("projects/42"),
        );
        assert_eq!(cached_project_id(&acc).as_deref(), Some("projects/42"));

        // A blank cached value is treated as "not discovered yet".
        acc.extra
            .insert(ANTIGRAVITY_PROJECT_KEY.into(), serde_json::json!(""));
        assert_eq!(cached_project_id(&acc), None);
    }

    #[test]
    fn base_url_matches_the_registry() {
        assert_eq!(
            base_url(provider::get("claude").unwrap()),
            "https://api.anthropic.com"
        );
        assert_eq!(
            base_url(provider::get("codex").unwrap()),
            "https://chatgpt.com/backend-api/codex"
        );
    }
}

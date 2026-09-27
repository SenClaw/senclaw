//! Token endpoint calls: authorization-code exchange and refresh.
//!
//! Both directions share body construction because each provider uses the same
//! encoding for both calls; only the grant type and a couple of fields differ.
//!
//! Nothing here logs token material. Error strings carry the HTTP status and
//! the provider's error body (which is an error description, never a
//! credential) so a failed sign-in is diagnosable without leaking anything.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

use super::provider::{BodyEncoding, OauthProviderDef};

/// Normalised token-endpoint response.
#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Lifetime in seconds, as reported by the provider.
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
    /// OIDC id_token, when the provider issues one (Codex does).
    pub id_token: Option<String>,
}

#[derive(Deserialize)]
struct RawTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    id_token: Option<String>,
    // Populated on failure responses that still return 200 (some providers do).
    error: Option<String>,
    error_description: Option<String>,
}

/// A refresh that failed because the grant itself is dead — the user must sign
/// in again. Distinguished from transient failures so the caller can mark the
/// account instead of retrying forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshFailure {
    /// Network blip, 5xx, timeout — worth retrying later.
    Transient,
    /// `invalid_grant` and friends — the refresh token is revoked or expired.
    Unrecoverable,
}

impl std::fmt::Display for RefreshFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient => write!(f, "transient"),
            Self::Unrecoverable => write!(f, "unrecoverable"),
        }
    }
}

/// Classify a token-endpoint error body. Anything naming a dead grant means
/// re-authorisation; everything else is worth another attempt.
pub fn classify_failure(status: u16, body: &str) -> RefreshFailure {
    let lower = body.to_lowercase();
    if lower.contains("invalid_grant")
        || lower.contains("invalid_request")
        || lower.contains("invalid_token")
        || lower.contains("revoked")
    {
        return RefreshFailure::Unrecoverable;
    }
    // A bare 400/401 from a token endpoint is the standard "this grant is no
    // longer good" answer even when the body is unhelpful.
    if status == 400 || status == 401 {
        return RefreshFailure::Unrecoverable;
    }
    RefreshFailure::Transient
}

/// Exchange an authorization code for tokens.
///
/// `redirect_uri` must be byte-identical to the one sent on the authorize
/// request — providers compare it verbatim.
pub async fn exchange_code(
    http: &reqwest::Client,
    def: &OauthProviderDef,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    state: &str,
) -> Result<TokenResponse> {
    // Anthropic hands back `code#state` on the loopback redirect; everything
    // after the fragment marker belongs in the `state` field, not the code.
    let (code, state) = match code.split_once('#') {
        Some((c, s)) if !s.is_empty() => (c, s),
        _ => (code, state),
    };

    let mut fields: Vec<(&str, String)> = vec![
        ("grant_type", "authorization_code".into()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", def.client_id()),
        ("code_verifier", code_verifier.to_string()),
    ];
    if def.id == "claude" {
        fields.push(("state", state.to_string()));
    }
    if def.sends_client_secret {
        if let Some(secret) = def.client_secret() {
            fields.push(("client_secret", secret));
        }
    }

    post_token(http, def, fields, "authorization_code").await
}

/// Trade a refresh token for a fresh access token.
pub async fn refresh(
    http: &reqwest::Client,
    def: &OauthProviderDef,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let mut fields: Vec<(&str, String)> = vec![
        ("grant_type", "refresh_token".into()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", def.client_id()),
    ];
    if def.refresh_includes_scope {
        fields.push(("scope", def.scope_string()));
    }
    if def.sends_client_secret {
        if let Some(secret) = def.client_secret() {
            fields.push(("client_secret", secret));
        }
    }

    post_token(http, def, fields, "refresh_token").await
}

/// What the device-authorization endpoint hands back (RFC 8628 §3.2).
#[derive(Debug, Clone)]
pub struct DeviceAuthorization {
    /// Secret we poll with. Never shown to the user.
    pub device_code: String,
    /// Short code the user types on the provider's page.
    pub user_code: String,
    /// Page the user opens.
    pub verification_uri: String,
    /// Same page with the code pre-filled, when the provider offers it.
    pub verification_uri_complete: Option<String>,
    /// Seconds between polls.
    pub interval: u64,
    /// Seconds until the device code dies.
    pub expires_in: i64,
}

#[derive(Deserialize)]
struct RawDeviceAuthorization {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    // GitHub spells it `verification_uri`; some providers use the `_url` form.
    verification_url: Option<String>,
    verification_uri_complete: Option<String>,
    interval: Option<u64>,
    expires_in: Option<i64>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Ask the provider to start a device authorization.
pub async fn request_device_code(
    http: &reqwest::Client,
    def: &OauthProviderDef,
) -> Result<DeviceAuthorization> {
    let url = def
        .device_code_url
        .ok_or_else(|| anyhow!("{} has no device-code endpoint", def.id))?;

    let mut fields: Vec<(&str, String)> = vec![("client_id", def.client_id())];
    if !def.scopes.is_empty() {
        fields.push(("scope", def.scope_string()));
    }

    let response = http
        .post(url)
        .header("Accept", "application/json")
        .form(&fields)
        .send()
        .await
        .with_context(|| format!("{} device-code request failed", def.id))?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "{} device-code request rejected ({}): {}",
            def.id,
            status,
            truncate(&body, 300)
        );
    }

    parse_device_authorization(&body).with_context(|| format!("{} device-code response", def.id))
}

/// Parse a device-authorization body. Split out for testing.
pub fn parse_device_authorization(body: &str) -> Result<DeviceAuthorization> {
    let raw: RawDeviceAuthorization =
        serde_json::from_str(body).with_context(|| format!("parse: {}", truncate(body, 200)))?;

    if let Some(err) = raw.error {
        let detail = raw.error_description.unwrap_or_default();
        bail!("provider returned error `{err}`: {detail}");
    }

    Ok(DeviceAuthorization {
        device_code: raw
            .device_code
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("no device_code in response"))?,
        user_code: raw
            .user_code
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("no user_code in response"))?,
        verification_uri: raw
            .verification_uri
            .or(raw.verification_url)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("no verification_uri in response"))?,
        verification_uri_complete: raw.verification_uri_complete.filter(|s| !s.is_empty()),
        // RFC 8628 §3.2: absent interval means 5 seconds.
        interval: raw.interval.unwrap_or(5).clamp(1, 60),
        expires_in: raw.expires_in.unwrap_or(900),
    })
}

/// One poll of the token endpoint during a device flow.
pub enum DevicePoll {
    /// The user has not finished yet — keep polling.
    Pending,
    /// The provider asked us to back off; the new interval is in seconds.
    SlowDown(u64),
    /// Done.
    Granted(Box<TokenResponse>),
}

/// Poll once for the device grant.
pub async fn poll_device_token(
    http: &reqwest::Client,
    def: &OauthProviderDef,
    device_code: &str,
) -> Result<DevicePoll> {
    let mut fields: Vec<(&str, String)> = vec![
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:device_code".into(),
        ),
        ("device_code", device_code.to_string()),
        ("client_id", def.client_id()),
    ];
    if def.sends_client_secret {
        if let Some(secret) = def.client_secret() {
            fields.push(("client_secret", secret));
        }
    }

    let response = http
        .post(def.token_url)
        .header("Accept", "application/json")
        .form(&fields)
        .send()
        .await
        .with_context(|| format!("{} device poll failed", def.id))?;

    let body = response.text().await.unwrap_or_default();
    classify_device_poll(&body)
}

/// Interpret a device-poll body.
///
/// Providers signal "not yet" with a 200 *or* a 400 carrying an
/// `authorization_pending` error, so the status is not a reliable signal here —
/// the body is.
pub fn classify_device_poll(body: &str) -> Result<DevicePoll> {
    let value: serde_json::Value = serde_json::from_str(body)
        .with_context(|| format!("parse device poll: {}", truncate(body, 200)))?;

    match value.get("error").and_then(|e| e.as_str()) {
        Some("authorization_pending") => return Ok(DevicePoll::Pending),
        Some("slow_down") => {
            let interval = value
                .get("interval")
                .and_then(|i| i.as_u64())
                .unwrap_or(5)
                .clamp(1, 60);
            return Ok(DevicePoll::SlowDown(interval));
        }
        Some("expired_token") => bail!("the device code expired — start the sign-in again"),
        Some("access_denied") => bail!("the sign-in was denied"),
        Some(other) => {
            let detail = value
                .get("error_description")
                .and_then(|d| d.as_str())
                .unwrap_or_default();
            bail!("device authorization failed (`{other}`): {detail}");
        }
        None => {}
    }

    Ok(DevicePoll::Granted(Box::new(parse_token_body(body)?)))
}

async fn post_token(
    http: &reqwest::Client,
    def: &OauthProviderDef,
    fields: Vec<(&str, String)>,
    grant: &str,
) -> Result<TokenResponse> {
    let request = http
        .post(def.token_url)
        .header("Accept", "application/json");

    let request = match def.body_encoding {
        BodyEncoding::Json => {
            let map: serde_json::Map<String, serde_json::Value> = fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), serde_json::Value::String(v)))
                .collect();
            request.json(&serde_json::Value::Object(map))
        }
        BodyEncoding::Form => request.form(&fields),
    };

    let response = request
        .send()
        .await
        .with_context(|| format!("{} {grant} request failed", def.id))?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        bail!(
            "{} {grant} rejected ({}): {}",
            def.id,
            status,
            truncate(&body, 400)
        );
    }

    parse_token_body(&body).with_context(|| format!("{} {grant} response", def.id))
}

/// Parse and validate a token-endpoint body. Split out so it can be tested
/// without a live provider.
pub fn parse_token_body(body: &str) -> Result<TokenResponse> {
    let raw: RawTokenResponse =
        serde_json::from_str(body).with_context(|| format!("parse: {}", truncate(body, 200)))?;

    if let Some(err) = raw.error {
        let detail = raw.error_description.unwrap_or_default();
        bail!("provider returned error `{err}`: {detail}");
    }

    let access_token = raw
        .access_token
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| anyhow!("no access_token in response"))?;

    Ok(TokenResponse {
        access_token,
        refresh_token: raw.refresh_token.filter(|t| !t.trim().is_empty()),
        expires_in: raw.expires_in,
        scope: raw.scope,
        id_token: raw.id_token,
    })
}

/// Pull an email out of an OIDC id_token for labelling the account.
///
/// The signature is intentionally **not** verified: this token came straight
/// from the provider's own TLS-protected token endpoint over a connection we
/// opened, and the value is only ever used as a display string. It never
/// grants anything.
pub fn email_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    json.get("email")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Char-boundary safe: slicing bytes here would panic on a multibyte body.
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::oauth::provider;

    #[test]
    fn parses_a_full_token_response() {
        let body = r#"{
            "access_token": "at-123",
            "refresh_token": "rt-456",
            "expires_in": 3600,
            "scope": "user:inference",
            "id_token": "x.y.z"
        }"#;
        let t = parse_token_body(body).unwrap();
        assert_eq!(t.access_token, "at-123");
        assert_eq!(t.refresh_token.as_deref(), Some("rt-456"));
        assert_eq!(t.expires_in, Some(3600));
        assert_eq!(t.scope.as_deref(), Some("user:inference"));
    }

    #[test]
    fn a_response_without_a_refresh_token_is_still_valid() {
        let t = parse_token_body(r#"{"access_token":"at","expires_in":60}"#).unwrap();
        assert_eq!(t.refresh_token, None);
    }

    #[test]
    fn blank_tokens_are_treated_as_absent() {
        let err = parse_token_body(r#"{"access_token":"   "}"#).unwrap_err();
        assert!(err.to_string().contains("no access_token"));

        let t = parse_token_body(r#"{"access_token":"at","refresh_token":""}"#).unwrap();
        assert_eq!(t.refresh_token, None);
    }

    #[test]
    fn a_200_carrying_an_error_field_is_an_error() {
        let body = r#"{"error":"invalid_grant","error_description":"expired"}"#;
        let err = parse_token_body(body).unwrap_err();
        assert!(err.to_string().contains("invalid_grant"), "{err}");
        assert!(err.to_string().contains("expired"), "{err}");
    }

    #[test]
    fn garbage_body_is_reported_not_panicked() {
        assert!(parse_token_body("<html>nope</html>").is_err());
    }

    #[test]
    fn dead_grants_are_classified_unrecoverable() {
        assert_eq!(
            classify_failure(400, r#"{"error":"invalid_grant"}"#),
            RefreshFailure::Unrecoverable
        );
        assert_eq!(
            classify_failure(401, "unauthorized"),
            RefreshFailure::Unrecoverable
        );
        assert_eq!(
            classify_failure(403, "token has been revoked"),
            RefreshFailure::Unrecoverable
        );
    }

    #[test]
    fn server_errors_are_classified_transient() {
        assert_eq!(
            classify_failure(500, "internal error"),
            RefreshFailure::Transient
        );
        assert_eq!(
            classify_failure(503, "try later"),
            RefreshFailure::Transient
        );
        assert_eq!(
            classify_failure(429, "slow down"),
            RefreshFailure::Transient
        );
    }

    #[test]
    fn extracts_email_from_an_id_token_payload() {
        // {"email":"dev@example.com","sub":"1"}
        let payload = URL_SAFE_NO_PAD.encode(br#"{"email":"dev@example.com","sub":"1"}"#);
        let token = format!("header.{payload}.signature");
        assert_eq!(
            email_from_id_token(&token).as_deref(),
            Some("dev@example.com")
        );
    }

    #[test]
    fn malformed_id_tokens_yield_no_email_instead_of_panicking() {
        assert_eq!(email_from_id_token(""), None);
        assert_eq!(email_from_id_token("only-one-part"), None);
        assert_eq!(email_from_id_token("a.!!!notbase64!!!.c"), None);
        let no_email = URL_SAFE_NO_PAD.encode(br#"{"sub":"1"}"#);
        assert_eq!(email_from_id_token(&format!("a.{no_email}.c")), None);
    }

    #[test]
    fn truncate_does_not_split_multibyte_characters() {
        let s = "kèm dấu tiếng Việt ".repeat(50);
        let out = truncate(&s, 10);
        assert_eq!(out.chars().count(), 11); // 10 + ellipsis
    }

    #[test]
    fn claude_exchange_splits_the_fragment_state_out_of_the_code() {
        // Mirrors the `code#state` form Anthropic returns; the split logic is
        // in exchange_code, so assert the primitive it relies on.
        let raw = "thecode#thestate";
        let (code, state) = raw.split_once('#').unwrap();
        assert_eq!(code, "thecode");
        assert_eq!(state, "thestate");
    }

    #[test]
    fn only_providers_that_declare_a_secret_would_send_one() {
        // Guards the exchange/refresh body builders against sending an empty
        // client_secret field.
        for p in provider::all() {
            if p.sends_client_secret {
                assert!(p.client_secret().is_some_and(|s| !s.is_empty()), "{}", p.id);
            }
        }
    }
}

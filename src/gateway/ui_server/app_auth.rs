//! Enforcing per-app access tokens on `/api/space/apps/<id>/…`.
//!
//! [`crate::apps::token`] mints the secrets and says what they mean; this
//! module is where a request is measured against one. Three questions, in
//! order:
//!
//! 1. **Does the caller declare an API version we can serve?** A version newer
//!    than this daemon's is refused rather than half-served (426).
//! 2. **Is the presented token real, and is it *this* app's?** A token for
//!    another app is 403 — never a redirect to the caller's own data, which
//!    would turn a bug into silent cross-app corruption.
//! 3. **When no token is presented, may the request proceed?** That is the
//!    `SENCLAW_APP_TOKEN_MODE` question: `off` (default) serves it, `warn`
//!    serves it and says so once per app, `strict` refuses it.
//!
//! # What "strict" actually buys
//!
//! Strict mode stops an app from *addressing* another app: its HTTP client has
//! one token, the token names one id, and every other id answers 403. It does
//! not stop a program that can read `~/.senclaw/senclaw.db`, because that
//! program can read every token in the table — and every app's data besides.
//! The boundary that makes strict mode meaningful is the per-app sandbox, which
//! is what keeps an app away from that file. Deployed without it, treat strict
//! mode as protection against app bugs and confused-deputy mistakes, not
//! against a hostile app.
//!
//! The daemon's own Web UI presents no app token — it manages *all* apps, so
//! scoping it to one would be wrong. It is recognised by the marks a browser
//! puts on a same-origin fetch (`Sec-Fetch-Site`, a loopback `Origin`), or by
//! the daemon API token when the daemon is bound beyond loopback. A determined
//! local process can set those headers too; see the paragraph above for why
//! that is the sandbox's problem and not this middleware's.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

use crate::apps::token::{self, TokenMode, HEADER_API_VERSION, HEADER_APP_TOKEN, MIN_API_VERSION};

use super::core::UiState;

/// The app a request proved it is, attached to the request for handlers that
/// want to log or narrow on it. Absent when the caller presented no token.
#[derive(Debug, Clone)]
pub struct AppIdentity(pub String);

/// Marker saying "this request came from an operator channel, not an app".
///
/// A *request extension*, deliberately — unlike a header, nothing on the
/// network can set one. The relay bridge attaches it because a relay frame is
/// already authenticated by relay pairing and synthesises a bare
/// `Request::builder()` with none of the marks a browser leaves, so strict mode
/// would otherwise refuse the phone app every app-scoped call.
#[derive(Debug, Clone, Copy)]
pub struct TrustedOperator;

/// Route suffixes under `/api/space/apps/<id>/` that serve **the app's own
/// data**: its settings, its database, its AI bridge, its MCP registration.
/// These are what strict mode requires a token for.
///
/// Management routes (`/start`, `/stop`, `/update`, `/sandbox`, `/runtime`, …)
/// are deliberately not here: they belong to the operator's UI, and an app has
/// no business calling them for *anyone*, including itself.
const SCOPED_SUFFIXES: &[&str] = &[
    "/bridge",
    "/config",
    "/sqlite/query",
    "/mcp/register",
    "/env",
    "/token",
];

/// Suffixes that carry a request *into* the app rather than into the daemon.
/// The proxy attaches the app's own token on the way out (see
/// `space::space_apps_proxy`), so requiring one on the way in would just block
/// the UI iframe from ever loading.
fn is_inbound_to_app(suffix: &str) -> bool {
    suffix.starts_with("/proxy") || suffix.starts_with("/static")
}

fn is_scoped_data_route(suffix: &str) -> bool {
    SCOPED_SUFFIXES
        .iter()
        .any(|s| suffix == *s || suffix.starts_with(&format!("{s}/")))
}

/// Split `/api/space/apps/<id>/<suffix>` into its two halves.
///
/// `None` for anything else, including the collection routes
/// (`/api/space/apps`, `/api/space/apps/register`) and the literal segments
/// that are not app ids (`/updates`, `/sandbox-overview`) — those are handled
/// by the daemon's own auth and have no app to scope to.
pub fn split_app_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("/api/space/apps/")?;
    let (id, suffix) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if id.is_empty() {
        return None;
    }
    // Literal siblings of `:id` in the router. They are collection endpoints,
    // not apps, and `space_apps_updates` would otherwise look like an app named
    // "updates".
    if matches!(
        id,
        "updates" | "register" | "register-local" | "install-zip" | "sandbox-overview" | "status"
    ) {
        return None;
    }
    Some((
        urlencoding::decode(id)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| id.to_string()),
        suffix.to_string(),
    ))
}

// ===== Credential extraction =====

/// The app token a request carries, from any of the three carriers:
/// `X-SenClaw-App-Token`, `Authorization: Bearer sca_…` (told apart from the
/// daemon token by its prefix), or `?app_token=` for clients that cannot set
/// headers.
pub fn presented_token(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    if let Some(v) = headers.get(HEADER_APP_TOKEN).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        {
            let t = t.trim();
            if token::looks_like_app_token(t) {
                return Some(t.to_string());
            }
        }
    }
    for pair in query.unwrap_or("").split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some("app_token") {
            let raw = it.next().unwrap_or("");
            if let Ok(decoded) = urlencoding::decode(raw) {
                let decoded = decoded.into_owned();
                if !decoded.is_empty() {
                    return Some(decoded);
                }
            }
        }
    }
    None
}

/// Whether this looks like the daemon's own Web UI rather than an app process.
///
/// A same-origin `fetch` from the SPA carries `Sec-Fetch-Site` (every browser
/// since 2020 sets it and page JS cannot forge it); the desktop shell and the
/// Vite dev server carry a loopback `Origin`; a remote operator carries the
/// daemon session cookie. An HTTP client in an app process carries none of
/// them unless it goes out of its way — which is exactly the "hostile app"
/// case the module docs put out of scope.
pub fn looks_like_daemon_ui(headers: &HeaderMap) -> bool {
    if headers.contains_key("sec-fetch-site") {
        return true;
    }
    if headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(super::auth::origin_is_loopback)
        .unwrap_or(false)
    {
        return true;
    }
    if headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .map(super::auth::origin_is_loopback)
        .unwrap_or(false)
    {
        return true;
    }
    // The cookie minted by POST /api/auth/login — a remote operator's browser
    // after the token handshake.
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|s| {
            s.split(';').any(|p| {
                p.trim()
                    .starts_with(&format!("{}=", super::auth::AUTH_COOKIE))
            })
        })
}

// ===== The decision =====

/// What the middleware concluded about one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Serve it. Carries the app this request proved it is, when it proved one.
    Allow(Option<String>),
    /// Refuse it: HTTP status, machine-readable code, human explanation.
    Deny(StatusCode, &'static str, String),
}

/// Whether this request may act on `app_id`, given what it presented.
///
/// Pure so the rules can be tested without a database or a server:
/// `owner_of` answers "whose token is this?" (`None` = not a real token), and
/// `is_daemon_ui` is [`looks_like_daemon_ui`]'s verdict.
pub fn decide(
    mode: TokenMode,
    app_id: &str,
    suffix: &str,
    presented: Option<&str>,
    owner_of: impl Fn(&str) -> Option<String>,
    is_daemon_ui: bool,
) -> Decision {
    if let Some(tok) = presented {
        return match owner_of(tok) {
            None => Decision::Deny(
                StatusCode::UNAUTHORIZED,
                "app_token_invalid",
                "unknown app access token — it may have been rotated. Restart the app so it \
                 picks up the current SENCLAW_TOKEN_ACCESS_APP."
                    .into(),
            ),
            // The whole point: a token names one app, and every other id is a
            // refusal rather than a silent redirect to the caller's own data.
            Some(owner) if owner != app_id => Decision::Deny(
                StatusCode::FORBIDDEN,
                "app_token_scope",
                format!(
                    "this access token belongs to app '{owner}', which may not act on app \
                     '{app_id}'"
                ),
            ),
            Some(owner) => Decision::Allow(Some(owner)),
        };
    }
    // No token. Only the app's own data routes are gated; the operator's
    // management controls and the routes that carry a request *into* the app
    // are not.
    if !is_scoped_data_route(suffix) || is_inbound_to_app(suffix) {
        return Decision::Allow(None);
    }
    match mode {
        TokenMode::Off | TokenMode::Warn => Decision::Allow(None),
        TokenMode::Strict if is_daemon_ui => Decision::Allow(None),
        TokenMode::Strict => Decision::Deny(
            StatusCode::UNAUTHORIZED,
            "app_token_required",
            format!(
                "SENCLAW_APP_TOKEN_MODE=strict: send app '{app_id}''s token in \
                 {HEADER_APP_TOKEN}. The daemon puts it in the app's environment as {}.",
                token::ENV_APP_TOKEN
            ),
        ),
    }
}

// ===== Version negotiation =====

/// The API version a request declares, if it declares one.
fn declared_version(headers: &HeaderMap) -> Option<Result<u32, ()>> {
    let raw = headers.get(HEADER_API_VERSION)?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(raw.parse::<u32>().map_err(|_| ()))
}

fn err(status: StatusCode, code: &str, message: String, daemon_version: u32) -> Response {
    (
        status,
        [(HEADER_API_VERSION, daemon_version.to_string())],
        Json(serde_json::json!({
            "error": message,
            "code": code,
            "apiVersion": daemon_version,
        })),
    )
        .into_response()
}

// ===== Middleware =====

/// Gate for every `/api/space/apps/<id>/…` route. Layered inside
/// `build_router`, so the relay bridge — which reuses the router without the
/// daemon's own token middleware — gets the same app scoping.
pub async fn app_auth_mw(State(s): State<Arc<UiState>>, req: Request, next: Next) -> Response {
    let daemon_version = s.config.space_api_version;
    let path = req.uri().path().to_string();
    let Some((app_id, suffix)) = split_app_path(&path) else {
        return next.run(req).await;
    };

    // 1. Version negotiation. A client asking for a contract this daemon does
    //    not implement is told so, rather than served a shape it will
    //    misparse. Older contracts are served: an app pinned to v1 predates
    //    tokens entirely and must keep working after a daemon upgrade.
    match declared_version(req.headers()) {
        Some(Err(())) => {
            return err(
                StatusCode::BAD_REQUEST,
                "bad_api_version",
                format!("{HEADER_API_VERSION} must be a whole number"),
                daemon_version,
            );
        }
        Some(Ok(v)) if v > daemon_version => {
            return err(
                StatusCode::UPGRADE_REQUIRED,
                "api_version_unsupported",
                format!(
                    "this app asks for Space API v{v}; this daemon serves v{daemon_version}. \
                     Update SenClaw, or pin the app's SDK to v{daemon_version}."
                ),
                daemon_version,
            );
        }
        Some(Ok(v)) if v < MIN_API_VERSION => {
            return err(
                StatusCode::BAD_REQUEST,
                "api_version_retired",
                format!("Space API v{v} is no longer served (minimum v{MIN_API_VERSION})"),
                daemon_version,
            );
        }
        _ => {}
    }

    let mut req = req;
    let presented = presented_token(req.headers(), req.uri().query());

    // 2. and 3. — whose token is it, and may a tokenless call proceed.
    let Some(db) = s.db.as_deref() else {
        // No database means no token table; refusing every app-scoped call over
        // it would take the whole app surface down for a problem that is not
        // the caller's.
        return next.run(req).await;
    };
    // Read live, not from the startup config: the operator can change this in
    // the UI and expects it to take effect without restarting the daemon.
    let (mode, _) = token::effective_mode(
        db,
        s.config.space_app_token_mode,
        s.config.space_app_token_mode_from_env,
    );
    let is_ui =
        req.extensions().get::<TrustedOperator>().is_some() || looks_like_daemon_ui(req.headers());
    match decide(
        mode,
        &app_id,
        &suffix,
        presented.as_deref(),
        |t| token::resolve(db, t),
        is_ui,
    ) {
        Decision::Allow(owner) => {
            if let Some(owner) = owner {
                req.extensions_mut().insert(AppIdentity(owner));
            } else if mode == TokenMode::Warn && is_scoped_data_route(&suffix) && !is_ui {
                warn_once(&app_id, &suffix);
            }
        }
        Decision::Deny(status, code, message) => {
            tracing::warn!("[app-auth] {status} {code} on {path}: {message}");
            return err(status, code, message, daemon_version);
        }
    }

    // Every app-scoped answer states the contract it was served under, so an
    // SDK can notice a daemon upgrade without a separate probe.
    let mut res = next.run(req).await;
    if let Ok(v) = daemon_version
        .to_string()
        .parse::<axum::http::HeaderValue>()
    {
        res.headers_mut().insert(HEADER_API_VERSION, v);
    }
    res
}

/// One line per app per daemon run. `warn` mode exists to find the apps that
/// have not adopted the token before flipping to `strict`; a line per request
/// would bury the log and tell the operator nothing new.
fn warn_once(app_id: &str, suffix: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut set) = seen.lock() else { return };
    if set.insert(app_id.to_string()) {
        tracing::warn!(
            "[app-auth] app '{app_id}' called {suffix} without an access token. \
             It would be refused under SENCLAW_APP_TOKEN_MODE=strict — update its SDK, \
             or restart it so it picks up {}.",
            token::ENV_APP_TOKEN
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn app_paths_split_into_id_and_suffix() {
        assert_eq!(
            split_app_path("/api/space/apps/kanban/bridge"),
            Some(("kanban".into(), "/bridge".into()))
        );
        assert_eq!(
            split_app_path("/api/space/apps/kanban/config/api-key"),
            Some(("kanban".into(), "/config/api-key".into()))
        );
        assert_eq!(
            split_app_path("/api/space/apps/kanban"),
            Some(("kanban".into(), "".into()))
        );
        // Percent-encoded ids resolve to the same string the router hands the
        // handler, or the scope comparison would fail on a legitimate call.
        assert_eq!(
            split_app_path("/api/space/apps/my%2Dapp/env"),
            Some(("my-app".into(), "/env".into()))
        );
    }

    #[test]
    fn collection_routes_are_not_apps() {
        assert_eq!(split_app_path("/api/space/apps"), None);
        assert_eq!(split_app_path("/api/chat/send"), None);
        // Literal siblings of `:id` — an app named "updates" does not exist,
        // and treating this as one would scope the operator's own listing.
        assert_eq!(split_app_path("/api/space/apps/updates"), None);
        assert_eq!(split_app_path("/api/space/apps/sandbox-overview"), None);
        assert_eq!(split_app_path("/api/space/apps/register"), None);
        assert_eq!(split_app_path("/api/space/apps/status"), None);
    }

    #[test]
    fn data_routes_are_scoped_and_management_routes_are_not() {
        assert!(is_scoped_data_route("/bridge"));
        assert!(is_scoped_data_route("/config"));
        assert!(is_scoped_data_route("/config/openai-key"));
        assert!(is_scoped_data_route("/sqlite/query"));
        assert!(is_scoped_data_route("/mcp/register"));
        assert!(is_scoped_data_route("/env"));
        // Not the app's data — the operator's controls.
        assert!(!is_scoped_data_route("/start"));
        assert!(!is_scoped_data_route("/stop"));
        assert!(!is_scoped_data_route("/runtime"));
        assert!(!is_scoped_data_route("/sandbox"));
        // `/mcp` (the info route) must not be swept in by `/mcp/register`.
        assert!(!is_scoped_data_route("/mcp"));
    }

    #[test]
    fn proxy_and_static_carry_requests_into_the_app() {
        assert!(is_inbound_to_app("/proxy"));
        assert!(is_inbound_to_app("/proxy/api/notes"));
        assert!(is_inbound_to_app("/static/index.html"));
        assert!(!is_inbound_to_app("/bridge"));
    }

    #[test]
    fn token_is_read_from_every_carrier() {
        let t = token::generate();
        assert_eq!(
            presented_token(&headers(&[(HEADER_APP_TOKEN, &t)]), None).as_deref(),
            Some(t.as_str())
        );
        assert_eq!(
            presented_token(&headers(&[("authorization", &format!("Bearer {t}"))]), None)
                .as_deref(),
            Some(t.as_str())
        );
        assert_eq!(
            presented_token(&HeaderMap::new(), Some(&format!("app_token={t}&x=1"))).as_deref(),
            Some(t.as_str())
        );
        assert_eq!(presented_token(&HeaderMap::new(), None), None);
    }

    #[test]
    fn the_daemon_token_is_not_mistaken_for_an_app_token() {
        // Both arrive as `Authorization: Bearer`. Without the prefix check the
        // operator's own token would be looked up as an app token, fail to
        // resolve, and 401 every remote request.
        let daemon_token = "a".repeat(64);
        assert_eq!(
            presented_token(
                &headers(&[("authorization", &format!("Bearer {daemon_token}"))]),
                None
            ),
            None
        );
    }

    #[test]
    fn browser_requests_are_recognised_as_the_daemon_ui() {
        assert!(looks_like_daemon_ui(&headers(&[(
            "sec-fetch-site",
            "same-origin"
        )])));
        assert!(looks_like_daemon_ui(&headers(&[(
            "origin",
            "http://127.0.0.1:18788"
        )])));
        assert!(looks_like_daemon_ui(&headers(&[(
            "referer",
            "http://localhost:5173/apps"
        )])));
        assert!(looks_like_daemon_ui(&headers(&[(
            "cookie",
            "senclaw_token=abc; other=1"
        )])));
        // A bare server-side HTTP client — what an app's SDK looks like.
        assert!(!looks_like_daemon_ui(&headers(&[(
            "accept",
            "application/json"
        )])));
        // A LAN page is not the daemon's UI.
        assert!(!looks_like_daemon_ui(&headers(&[(
            "origin",
            "http://192.168.1.20:3000"
        )])));
    }

    /// Two apps exist: `alpha` holds `sca_aaa…`, `beta` holds `sca_bbb…`.
    fn owner_of(token: &str) -> Option<String> {
        match token {
            t if t == format!("{}{}", token::TOKEN_PREFIX, "a".repeat(64)) => Some("alpha".into()),
            t if t == format!("{}{}", token::TOKEN_PREFIX, "b".repeat(64)) => Some("beta".into()),
            _ => None,
        }
    }

    fn alpha_token() -> String {
        format!("{}{}", token::TOKEN_PREFIX, "a".repeat(64))
    }

    fn beta_token() -> String {
        format!("{}{}", token::TOKEN_PREFIX, "b".repeat(64))
    }

    #[test]
    fn an_app_reaches_its_own_data() {
        let d = decide(
            TokenMode::Strict,
            "alpha",
            "/bridge",
            Some(&alpha_token()),
            owner_of,
            false,
        );
        assert_eq!(d, Decision::Allow(Some("alpha".into())));
    }

    #[test]
    fn an_app_cannot_reach_another_apps_data() {
        // The bug this feature exists to close: before it, beta's process could
        // POST /api/space/apps/alpha/bridge — a full tool-enabled agent — and
        // read alpha's config, where its API keys live.
        for mode in [TokenMode::Off, TokenMode::Warn, TokenMode::Strict] {
            let d = decide(
                mode,
                "alpha",
                "/bridge",
                Some(&beta_token()),
                owner_of,
                false,
            );
            match d {
                Decision::Deny(status, code, _) => {
                    assert_eq!(status, StatusCode::FORBIDDEN);
                    assert_eq!(code, "app_token_scope");
                }
                other => panic!("{mode} mode allowed a cross-app call: {other:?}"),
            }
        }
    }

    #[test]
    fn a_rotated_token_stops_working_in_every_mode() {
        // Scoping is not a mode: presenting a token that resolves to nothing is
        // refused even with enforcement off, or `off` would be a way to bypass
        // a rotation.
        for mode in [TokenMode::Off, TokenMode::Warn, TokenMode::Strict] {
            let stale = format!("{}{}", token::TOKEN_PREFIX, "c".repeat(64));
            match decide(mode, "alpha", "/config", Some(&stale), owner_of, false) {
                Decision::Deny(status, code, _) => {
                    assert_eq!(status, StatusCode::UNAUTHORIZED);
                    assert_eq!(code, "app_token_invalid");
                }
                other => panic!("{mode} mode accepted a dead token: {other:?}"),
            }
        }
    }

    #[test]
    fn off_mode_serves_a_tokenless_call_exactly_as_before() {
        // Every app installed before this feature calls without a token. The
        // default must not break them.
        assert_eq!(
            decide(TokenMode::Off, "alpha", "/bridge", None, owner_of, false),
            Decision::Allow(None)
        );
        assert_eq!(
            decide(
                TokenMode::Warn,
                "alpha",
                "/sqlite/query",
                None,
                owner_of,
                false
            ),
            Decision::Allow(None)
        );
    }

    #[test]
    fn strict_mode_refuses_a_tokenless_data_call() {
        match decide(
            TokenMode::Strict,
            "alpha",
            "/config/key",
            None,
            owner_of,
            false,
        ) {
            Decision::Deny(status, code, msg) => {
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert_eq!(code, "app_token_required");
                // The message has to name the variable, or the operator has
                // nothing to act on.
                assert!(msg.contains(token::ENV_APP_TOKEN), "{msg}");
            }
            other => panic!("strict mode served a tokenless data call: {other:?}"),
        }
    }

    #[test]
    fn strict_mode_still_serves_the_operators_own_ui() {
        // The Web UI manages every app, so scoping it to one would be wrong —
        // and it holds no app token to present.
        assert_eq!(
            decide(TokenMode::Strict, "alpha", "/config", None, owner_of, true),
            Decision::Allow(None)
        );
    }

    #[test]
    fn strict_mode_leaves_management_and_proxy_routes_alone() {
        // Starting an app, opening its iframe, and loading its static assets
        // are the operator's, not the app's — gating them on a token the
        // browser does not have would just break the UI.
        for suffix in [
            "/start",
            "/stop",
            "/runtime",
            "/proxy/api/notes",
            "/static/index.html",
        ] {
            assert_eq!(
                decide(TokenMode::Strict, "alpha", suffix, None, owner_of, false),
                Decision::Allow(None),
                "{suffix} must not require a token"
            );
        }
    }

    #[test]
    fn version_header_parsing() {
        assert_eq!(declared_version(&HeaderMap::new()), None);
        assert_eq!(
            declared_version(&headers(&[(HEADER_API_VERSION, "2")])),
            Some(Ok(2))
        );
        assert_eq!(
            declared_version(&headers(&[(HEADER_API_VERSION, "v2")])),
            Some(Err(()))
        );
    }
}

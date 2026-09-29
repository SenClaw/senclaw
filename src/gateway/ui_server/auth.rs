//! API access-token auth for the daemon's own HTTP + WS surface.
//!
//! Threat model: the daemon normally binds `127.0.0.1` and trusts the local
//! machine. When the user opts into LAN exposure (`SENCLAW_UI_BIND_HOST=0.0.0.0`)
//! every `/api/*` route and the WebSocket gateway become reachable from the
//! network, so **non-loopback peers must present the API token**. Loopback
//! peers stay exempt — the bundled desktop app, Space Apps calling back into
//! the daemon, and same-machine tooling keep working with zero configuration.
//!
//! That exemption is *wrong* for one common deployment: a TLS-terminating
//! reverse proxy on the same host. Every Internet client then arrives from
//! `127.0.0.1` and the peer address stops being evidence of anything, so the
//! policy is a tri-state [`AuthMode`] rather than a boolean derived from the
//! bind host:
//!
//! - `auto` (default) — required exactly when the bind host is not loopback;
//!   loopback peers exempt. The historical behaviour.
//! - `always` — every peer presents the token, loopback included. This is the
//!   cloud / reverse-proxy / Docker answer, and it needs no trust in a
//!   forwarded-for header.
//! - `off` — never required (an already-authenticated ingress in front).
//!
//! `SENCLAW_AUTH_MODE` sets it at startup; the operator can change it live at
//! `PUT /api/auth/mode`, which stores the choice in `router_state` and wins
//! over the environment. An unrecognised value falls back to `auto`, never to
//! `off` — a typo must not silently disable the gate.
//!
//! The token is resolved once at startup: `SENCLAW_API_TOKEN` env override,
//! else `~/.senclaw/api_token` (auto-generated on first use, chmod 0600).
//!
//! Accepted credential carriers, in order of preference:
//! - `Authorization: Bearer <token>`
//! - `X-SenClaw-Token: <token>`
//! - `?token=<token>` query parameter (WebSocket clients that cannot set headers)
//! - `senclaw_token` cookie — set by `POST /api/auth/login`. Required for the
//!   browser flows that cannot attach a header: Space-App proxy iframes and
//!   WS upgrades. `SameSite=Lax` keeps cross-site pages from riding it.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use rand::RngCore;

/// Cookie set by `POST /api/auth/login`.
pub const AUTH_COOKIE: &str = "senclaw_token";

/// Paths under `/api/` that must stay reachable without a token: the login
/// handshake itself, and the probe the web/desktop gates use to decide
/// whether to even show a token prompt.
pub(crate) const OPEN_API_PATHS: &[&str] = &["/api/auth/login", "/api/auth/status"];

// ===== Policy =====

/// When the daemon demands its API token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Required exactly when the bind host is not loopback; loopback peers
    /// exempt. What the daemon did before the mode existed.
    Auto,
    /// Required from every peer, loopback included. The only correct setting
    /// behind a same-host reverse proxy, because the proxy makes every remote
    /// client look local.
    Always,
    /// Never required. For an ingress that already authenticates, or a
    /// container network the operator trusts end to end.
    Off,
}

/// The mode a daemon with no configuration runs in.
pub const DEFAULT_AUTH_MODE: AuthMode = AuthMode::Auto;

impl AuthMode {
    /// Parse an explicitly written mode; `None` for anything unrecognised so
    /// the caller decides what a typo means.
    pub fn parse_opt(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "always" | "on" | "require" | "required" | "strict" => Some(Self::Always),
            "off" | "none" | "disabled" => Some(Self::Off),
            _ => None,
        }
    }

    /// Parse an environment value. An unrecognised spelling falls back to
    /// [`DEFAULT_AUTH_MODE`] — **never** to `Off`: a typo in
    /// `SENCLAW_AUTH_MODE` must not silently open the daemon up.
    pub fn from_env_value(raw: &str) -> Self {
        match Self::parse_opt(raw) {
            Some(m) => m,
            None => {
                tracing::warn!(
                    "[Auth] unknown SENCLAW_AUTH_MODE {raw:?} — falling back to \"auto\" \
                     (expected auto, always or off)"
                );
                DEFAULT_AUTH_MODE
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Off => "off",
        }
    }
}

/// Where the mode in force came from — the three sources behave differently
/// when the operator tries to change it, so the UI has to say which one won.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeSource {
    /// Chosen in the UI; stored in the database and wins over the environment.
    Ui,
    /// `SENCLAW_AUTH_MODE` in the daemon's environment.
    Env,
    /// Neither — [`DEFAULT_AUTH_MODE`].
    Default,
}

impl ModeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Env => "env",
            Self::Default => "default",
        }
    }
}

/// KV key holding the operator's chosen mode. `router_state` rather than a
/// table of its own: one scalar does not justify a migration to maintain
/// forever (same call as `space:appTokenMode`).
const MODE_KEY: &str = "auth:mode";

/// The chosen mode, cached. `None` = not read yet; `Some(None)` = read, and
/// nothing was chosen, so the environment decides. Cached because every single
/// request asks, and the answer changes only when someone clicks a button.
fn mode_cache() -> &'static RwLock<Option<Option<AuthMode>>> {
    static C: OnceLock<RwLock<Option<Option<AuthMode>>>> = OnceLock::new();
    C.get_or_init(|| RwLock::new(None))
}

/// The mode the operator chose, or `None` to follow the environment.
pub fn mode_override(db: &crate::db::Db) -> Option<AuthMode> {
    if let Ok(c) = mode_cache().read() {
        if let Some(cached) = *c {
            return cached;
        }
    }
    let found = db
        .get_router_state(MODE_KEY)
        .ok()
        .flatten()
        .and_then(|raw| AuthMode::parse_opt(&raw));
    if let Ok(mut c) = mode_cache().write() {
        *c = Some(found);
    }
    found
}

/// Choose a mode, or pass `None` to hand the decision back to the environment.
pub fn set_mode_override(db: &crate::db::Db, mode: Option<AuthMode>) -> anyhow::Result<()> {
    match mode {
        Some(m) => db.set_router_state(MODE_KEY, m.as_str())?,
        None => db.delete_router_state(MODE_KEY)?,
    }
    if let Ok(mut c) = mode_cache().write() {
        *c = Some(mode);
    }
    Ok(())
}

/// Forget the cached choice. Tests only — the daemon has one database.
#[cfg(test)]
fn mode_cache_clear() {
    if let Ok(mut c) = mode_cache().write() {
        *c = None;
    }
}

#[derive(Clone)]
pub struct ApiAuth {
    /// Policy read from `SENCLAW_AUTH_MODE` at startup.
    pub env_mode: AuthMode,
    /// Whether that variable was actually set. A value that merely *equals*
    /// the default must not be reported to the UI as configured.
    pub env_set: bool,
    /// True when the daemon's bind host only ever resolves to this machine —
    /// exactly what [`AuthMode::Auto`] keys off.
    pub bind_is_loopback: bool,
    /// The accepted token. Always `Some` outside bare test setups.
    pub token: Option<String>,
    /// Where the token is persisted, so the operator can be told where to look.
    /// Never the value itself.
    pub token_path: Option<PathBuf>,
    /// `Secure` on the session cookie. `None` = infer per request from
    /// `X-Forwarded-Proto`, which is what a TLS-terminating proxy sets. Getting
    /// this wrong in either direction breaks login silently: `Secure` on plain
    /// HTTP makes the browser drop the cookie, and its absence over HTTPS
    /// leaks the token to a downgrade.
    pub cookie_secure: Option<bool>,
    /// Backs the live override in `router_state`. `None` (tests, the relay
    /// bridge) means the environment alone decides.
    pub db: Option<Arc<crate::db::Db>>,
}

impl ApiAuth {
    /// Auth disabled — for bare test setups and the relay bridge, which
    /// authenticates by relay pairing instead.
    pub fn disabled() -> Self {
        Self {
            env_mode: AuthMode::Off,
            env_set: false,
            bind_is_loopback: true,
            token: None,
            token_path: None,
            cookie_secure: None,
            db: None,
        }
    }

    /// The mode in force, and where it came from.
    pub fn effective_mode(&self) -> (AuthMode, ModeSource) {
        if let Some(db) = self.db.as_deref() {
            if let Some(m) = mode_override(db) {
                return (m, ModeSource::Ui);
            }
        }
        if self.env_set {
            (self.env_mode, ModeSource::Env)
        } else {
            (self.env_mode, ModeSource::Default)
        }
    }

    /// Whether a token is demanded at all right now.
    pub fn required(&self) -> bool {
        Self::mode_requires(self.effective_mode().0, self.bind_is_loopback)
    }

    fn mode_requires(mode: AuthMode, bind_is_loopback: bool) -> bool {
        match mode {
            AuthMode::Off => false,
            AuthMode::Always => true,
            AuthMode::Auto => !bind_is_loopback,
        }
    }
}

// ===== Host / origin classification =====

/// True for hosts that only ever resolve to the local machine:
/// `localhost`, `127.0.0.0/8`, `::1` (with or without brackets).
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim();
    let h = h.strip_prefix('[').unwrap_or(h);
    let h = h.strip_suffix(']').unwrap_or(h);
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    h.parse::<IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

/// True when an `Origin` header value (`scheme://host[:port]`) points at a
/// loopback host. Non-URL origins (`null`, garbage) are rejected.
pub fn origin_is_loopback(origin: &str) -> bool {
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    let Some(rest) = rest else { return false };
    let host_port = rest.split('/').next().unwrap_or("");
    // `[::1]:5173` — the port separator is the last ':' *after* any ']'.
    let host = if let Some(end) = host_port.rfind(']') {
        &host_port[..=end]
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    is_loopback_host(host)
}

/// The host a request names (`Host`, else the URI authority), port removed.
fn request_host(req: &Request) -> Option<String> {
    let raw = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.to_string()))?;
    // `[::1]:18788` — the port separator is the last ':' *after* any ']'.
    let host = match raw.rfind(']') {
        Some(end) => raw[..=end].to_string(),
        None => raw.split(':').next().unwrap_or("").to_string(),
    };
    Some(host)
}

/// Whether a request may ride the trust given to this machine (no token in
/// `auto`): it names this machine the way local clients do, and no web page
/// of another site sent it.
///
/// A browser on this machine reaching the daemon under another name — DNS
/// rebinding, `lvh.me`, `localhost.` — sends that name as `Host`, and pages
/// under it belong to another site. A page of another site calling
/// `127.0.0.1` sends its own `Origin`, and nothing like CORS stands between a
/// page and a WebSocket: without this, any page in any browser here — the
/// agent's own included — could open the gateway and answer permission
/// prompts. Native clients (the desktop app, CLI, MCP servers, Space Apps
/// over reqwest/undici) send no `Origin`; a request with no `Host` is not
/// from a browser.
fn local_request(req: &Request) -> bool {
    if request_host(req).is_some_and(|h| !is_loopback_host(&h)) {
        return false;
    }
    let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    if origin.starts_with("chrome-extension://") {
        // Only the extension channels, which check the extension id and pair it.
        return req.uri().path().starts_with("/browser");
    }
    origin_is_loopback(origin)
}

// ===== Token resolution =====

/// Resolve the daemon API token: env override first, else the persisted
/// `api_token` file next to the global config (created on first use).
pub fn resolve_token(env_token: Option<&str>, senclaw_dir: &Path) -> String {
    if let Some(t) = env_token {
        let t = t.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    load_or_create_token_file(&senclaw_dir.join("api_token"))
}

fn load_or_create_token_file(path: &PathBuf) -> String {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let t = existing.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    let token: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(path, &token) {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        }
        Err(e) => {
            // In-memory token still protects this run; it just won't survive
            // a restart.
            tracing::warn!("[Auth] cannot persist API token at {path:?}: {e}");
        }
    }
    token
}

// ===== Credential extraction & checking =====

/// Constant-time string equality — a naive `==` short-circuits on the first
/// differing byte and leaks prefix length through timing.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(t) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")) {
            let t = t.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    if let Some(v) = headers.get("x-senclaw-token").and_then(|v| v.to_str().ok()) {
        let t = v.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

fn token_from_query(query: Option<&str>) -> Option<String> {
    for pair in query?.split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some("token") {
            let raw = it.next().unwrap_or("");
            let decoded = urlencoding::decode(raw).map(|c| c.into_owned()).ok()?;
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

fn token_from_cookies(headers: &HeaderMap) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(s) = value.to_str() else { continue };
        for part in s.split(';') {
            let mut it = part.trim().splitn(2, '=');
            if it.next() == Some(AUTH_COOKIE) {
                let v = it.next().unwrap_or("").trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Whether this request may pass. Under [`AuthMode::Auto`] loopback peers are
/// trusted and remote peers must carry the token in a header, the query string,
/// or the cookie; under [`AuthMode::Always`] nobody is trusted on address
/// alone. `peer == None` (no `ConnectInfo`, e.g. unit tests without a real
/// socket) is treated as remote — fail closed.
pub fn authorize(auth: &ApiAuth, peer: Option<SocketAddr>, req: &Request) -> bool {
    let mode = auth.effective_mode().0;
    if mode == AuthMode::Off {
        return true;
    }
    let local = local_request(req);
    if !ApiAuth::mode_requires(mode, auth.bind_is_loopback) && local {
        return true;
    }
    // The peer address is only evidence while nothing rewrites it. A
    // TLS-terminating proxy on this host makes every Internet client arrive
    // from 127.0.0.1, which is precisely what `Always` exists to survive.
    if mode != AuthMode::Always && local {
        if let Some(p) = peer {
            if p.ip().is_loopback() {
                return true;
            }
        }
    }
    let Some(expected) = auth.token.as_deref() else {
        return false;
    };
    let given = token_from_headers(req.headers())
        .or_else(|| token_from_query(req.uri().query()))
        .or_else(|| token_from_cookies(req.headers()));
    match given {
        Some(t) => ct_eq(&t, expected),
        None => false,
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "unauthorized",
            "authRequired": true,
        })),
    )
        .into_response()
}

// ===== Middleware =====

/// UI server (18788): gate `/api/*` except the login/status handshake.
/// Static assets and the SPA shell stay open — the remote user must be able
/// to load the page that asks for the token.
pub async fn http_auth_mw(
    State(auth): State<Arc<ApiAuth>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let protected = path.starts_with("/api/") && !OPEN_API_PATHS.contains(&path);
    if protected && !authorize(&auth, peer.map(|c| c.0), &req) {
        return unauthorized();
    }
    next.run(req).await
}

/// WS gateway (18789): gate **every** path at upgrade time. The in-band
/// `connect` message is not a real gate — the dispatcher runs handlers for
/// unauthenticated sockets — so the HTTP upgrade is where remote peers are
/// stopped. Browsers authenticate via the `senclaw_token` cookie (cookies are
/// port-agnostic, so the login on 18788 covers 18789 on the same host);
/// native clients use `?token=` or headers.
pub async fn ws_auth_mw(
    State(auth): State<Arc<ApiAuth>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    req: Request,
    next: Next,
) -> Response {
    if !authorize(&auth, peer.map(|c| c.0), &req) {
        return unauthorized();
    }
    next.run(req).await
}

// ===== /api/auth/* handlers =====

#[derive(serde::Deserialize)]
pub struct LoginBody {
    pub token: String,
}

/// `POST /api/auth/login {token}` — verify the token and mint the browser
/// session cookie. Open (unauthenticated) by design; it *is* the login.
pub async fn auth_login(
    State(auth): State<Arc<ApiAuth>>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    if !auth.required() {
        return Json(serde_json::json!({ "ok": true, "authRequired": false })).into_response();
    }
    let ok = auth
        .token
        .as_deref()
        .map(|t| ct_eq(body.token.trim(), t))
        .unwrap_or(false);
    if !ok {
        return unauthorized();
    }
    // HttpOnly keeps page JS away from it; SameSite=Lax blocks cross-site use.
    // `Secure` is conditional, and both mistakes are silent: setting it on the
    // plain-HTTP LAN deployment makes the browser discard a cookie the login
    // just "succeeded" in minting, and omitting it behind TLS lets a downgrade
    // carry the token in clear.
    let secure = auth
        .cookie_secure
        .unwrap_or_else(|| forwarded_proto_is_https(&headers));
    let cookie = format!(
        "{AUTH_COOKIE}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000{}",
        body.token.trim(),
        if secure { "; Secure" } else { "" }
    );
    (
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "ok": true, "authRequired": true })),
    )
        .into_response()
}

/// `GET /api/auth/status` — lets a client decide whether to prompt for a
/// token before touching any gated endpoint. Open by design; leaks only the
/// two booleans.
pub async fn auth_status(
    State(auth): State<Arc<ApiAuth>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    req: Request,
) -> Json<serde_json::Value> {
    let authorized = authorize(&auth, peer.map(|c| c.0), &req);
    let (mode, source) = auth.effective_mode();
    Json(serde_json::json!({
        "authRequired": auth.required(),
        "authorized": authorized,
        "mode": mode.as_str(),
        "modeSource": source.as_str(),
    }))
}

/// True when a TLS-terminating proxy in front says the client leg was HTTPS.
/// Only ever *adds* protection (the `Secure` cookie flag), so a spoofed header
/// cannot weaken anything — it can at worst make a plain-HTTP client's cookie
/// undeliverable.
fn forwarded_proto_is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("https")
        })
        .unwrap_or(false)
}

// ===== /api/auth/mode =====

/// `GET /api/auth/mode` — the switch itself, for the settings UI.
///
/// Gated like every other `/api/` route on purpose: an anonymous remote client
/// must not be able to read, let alone flip, the daemon's own gate.
pub async fn auth_mode_get(State(auth): State<Arc<ApiAuth>>) -> Json<serde_json::Value> {
    let (mode, source) = auth.effective_mode();
    Json(serde_json::json!({
        "mode": mode.as_str(),
        "source": source.as_str(),
        // What the daemon falls back to if the UI choice is cleared — the
        // label the "follow the environment" option needs to show.
        "envMode": auth.env_mode.as_str(),
        "envSet": auth.env_set,
        "defaultMode": DEFAULT_AUTH_MODE.as_str(),
        "required": auth.required(),
        "bindIsLoopback": auth.bind_is_loopback,
        // The path, never the value: the operator has to be told where to read
        // the token they are about to need.
        "tokenPath": auth.token_path.as_ref().map(|p| p.display().to_string()),
        // No database ⇒ no live override is possible; the UI must not offer a
        // switch that silently does nothing.
        "canOverride": auth.db.is_some(),
    }))
}

#[derive(serde::Deserialize)]
pub struct AuthModeBody {
    /// `auto` | `always` | `off`, or absent/null to follow the environment.
    #[serde(default)]
    pub mode: Option<String>,
}

/// `PUT /api/auth/mode` — change it live, no restart: the middleware reads the
/// override on every request.
///
/// Switching to `always` from a loopback session locks that session out on its
/// very next call — by design, and the reason the response repeats where the
/// token file lives so the client can log straight back in.
pub async fn auth_mode_put(
    State(auth): State<Arc<ApiAuth>>,
    Json(body): Json<AuthModeBody>,
) -> Response {
    let Some(db) = auth.db.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "no database — this daemon cannot store an auth-mode override"
            })),
        )
            .into_response();
    };
    let chosen = match body.mode.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        None => None,
        Some(raw) => match AuthMode::parse_opt(raw) {
            Some(m) => Some(m),
            // Never coerce: a typo would set a gate the operator did not ask
            // for while they believe they did.
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!("Unknown mode {raw:?} — expected auto, always or off")
                    })),
                )
                    .into_response()
            }
        },
    };
    if let Err(e) = set_mode_override(db, chosen) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    auth_mode_get(State(auth)).await.into_response()
}

// ===== CORS =====

/// Replacement for the old `CorsLayer::permissive()` (ACAO `*`), which let
/// any web page the user visited read API responses off the loopback daemon —
/// including cleartext provider keys from `/api/llm-config`. Only loopback
/// origins (the Vite dev server, local tooling) may now read cross-origin
/// responses; the served UI itself is same-origin and needs no CORS at all.
pub fn restrictive_cors() -> tower_http::cors::CorsLayer {
    use axum::http::HeaderName;
    tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::AllowOrigin::predicate(|origin, _| {
            origin.to_str().map(origin_is_loopback).unwrap_or(false)
        }))
        .allow_methods(tower_http::cors::Any)
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            HeaderName::from_static("x-senclaw-token"),
        ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn req(uri: &str) -> Request {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn req_with_header(uri: &str, name: &str, value: &str) -> Request {
        Request::builder()
            .uri(uri)
            .header(name, value)
            .body(Body::empty())
            .unwrap()
    }

    /// `auto` mode on a LAN-exposed daemon — the historical posture.
    fn auth_on(token: &str) -> ApiAuth {
        ApiAuth {
            env_mode: AuthMode::Auto,
            env_set: false,
            bind_is_loopback: false,
            token: Some(token.to_string()),
            token_path: None,
            cookie_secure: None,
            db: None,
        }
    }

    /// `always` mode: nobody is trusted on address alone.
    fn auth_always(token: &str) -> ApiAuth {
        ApiAuth {
            env_mode: AuthMode::Always,
            env_set: true,
            // Loopback bind *and* still required — the reverse-proxy shape.
            bind_is_loopback: true,
            token: Some(token.to_string()),
            token_path: None,
            cookie_secure: None,
            db: None,
        }
    }

    fn remote_peer() -> Option<SocketAddr> {
        Some("192.168.1.50:44444".parse().unwrap())
    }

    fn local_peer() -> Option<SocketAddr> {
        Some("127.0.0.1:55555".parse().unwrap())
    }

    fn local_auto() -> ApiAuth {
        ApiAuth { bind_is_loopback: true, ..auth_on("tok") }
    }

    fn built(uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut b = Request::builder().uri(uri);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    /// The default posture trusts this machine, but only callers that name it
    /// as local clients do: another name (DNS rebinding, an alias that
    /// resolves to 127.0.0.1) or another site's page gets no free pass.
    #[test]
    fn local_trust_needs_a_local_host_and_no_foreign_page() {
        let auth = local_auto();
        let peer = local_peer();
        let ok = |uri: &str, headers: &[(&str, &str)]| authorize(&auth, peer, &built(uri, headers));
        assert!(ok("/api/config", &[("host", "127.0.0.1:18788")]));
        assert!(ok("/api/config", &[("host", "localhost:18788"), ("origin", "http://localhost:18788")]));
        assert!(ok("/api/config", &[("host", "[::1]:18788"), ("origin", "http://127.0.0.1:5173")]), "the dev server");
        assert!(ok("/api/config", &[]), "no Host: not a browser");
        for host in ["lvh.me:18788", "localhost.:18788", "[::ffff:127.0.0.1]:18788", "evil.test:18788"] {
            assert!(!ok("/api/llm-config", &[("host", host)]), "{host} rode the local trust");
            let with_token = [("host", host), ("authorization", "Bearer tok")];
            assert!(ok("/api/llm-config", &with_token), "{host} with the token");
        }
        // Another site's page calling the loopback address (fetch or WebSocket).
        assert!(!ok("/", &[("host", "127.0.0.1:18789"), ("origin", "https://evil.test")]));
        assert!(!ok("/", &[("host", "127.0.0.1:18789"), ("origin", "null")]));
        // Extensions only on the extension channels, which pair them.
        assert!(ok("/browser/ext", &[("host", "127.0.0.1:18789"), ("origin", "chrome-extension://abcdefghijklmnopabcdefghijklmnop")]));
        assert!(!ok("/", &[("host", "127.0.0.1:18789"), ("origin", "chrome-extension://abcdefghijklmnopabcdefghijklmnop")]));
        assert!(!ok("/api/browser-agent/approvals/apv_1", &[("host", "127.0.0.1:18788"), ("origin", "chrome-extension://abcdefghijklmnopabcdefghijklmnop")]));
    }

    #[test]
    fn loopback_hosts() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.5.0.3"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.10"));
        assert!(!is_loopback_host("example.com"));
        assert!(!is_loopback_host(""));
    }

    #[test]
    fn loopback_origins() {
        assert!(origin_is_loopback("http://127.0.0.1:5173"));
        assert!(origin_is_loopback("http://localhost:18788"));
        assert!(origin_is_loopback("http://localhost"));
        assert!(origin_is_loopback("https://[::1]:8443"));
        assert!(!origin_is_loopback("http://192.168.1.7:5173"));
        assert!(!origin_is_loopback("https://evil.example"));
        assert!(!origin_is_loopback("null"));
        assert!(!origin_is_loopback("file://x"));
        // Loopback host as a *subdomain* of a public domain must not pass.
        assert!(!origin_is_loopback("http://localhost.evil.example"));
    }

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(!ct_eq("", "a"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn disabled_auth_allows_everything() {
        let auth = ApiAuth::disabled();
        assert!(authorize(&auth, remote_peer(), &req("/api/llm-config")));
        assert!(authorize(&auth, None, &req("/api/llm-config")));
    }

    #[test]
    fn loopback_peer_is_exempt() {
        let auth = auth_on("secret");
        assert!(authorize(&auth, local_peer(), &req("/api/llm-config")));
    }

    #[test]
    fn remote_peer_needs_token() {
        let auth = auth_on("secret");
        assert!(!authorize(&auth, remote_peer(), &req("/api/llm-config")));
        // Missing ConnectInfo fails closed.
        assert!(!authorize(&auth, None, &req("/api/llm-config")));
    }

    #[test]
    fn bearer_header_accepted() {
        let auth = auth_on("secret");
        let r = req_with_header("/api/x", "authorization", "Bearer secret");
        assert!(authorize(&auth, remote_peer(), &r));
        let r = req_with_header("/api/x", "authorization", "Bearer wrong");
        assert!(!authorize(&auth, remote_peer(), &r));
    }

    #[test]
    fn custom_header_accepted() {
        let auth = auth_on("secret");
        let r = req_with_header("/api/x", "x-senclaw-token", "secret");
        assert!(authorize(&auth, remote_peer(), &r));
    }

    #[test]
    fn query_token_accepted() {
        let auth = auth_on("se cret");
        let r = req("/api/ws/terminal?cwd=%2Ftmp&token=se%20cret");
        assert!(authorize(&auth, remote_peer(), &r));
        let r = req("/api/ws/terminal?token=wrong");
        assert!(!authorize(&auth, remote_peer(), &r));
    }

    #[test]
    fn cookie_accepted() {
        let auth = auth_on("secret");
        let r = req_with_header("/api/x", "cookie", "theme=dark; senclaw_token=secret");
        assert!(authorize(&auth, remote_peer(), &r));
        let r = req_with_header("/api/x", "cookie", "senclaw_token=wrong");
        assert!(!authorize(&auth, remote_peer(), &r));
    }

    #[test]
    fn token_file_roundtrip_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let t1 = resolve_token(None, dir.path());
        assert_eq!(t1.len(), 64, "32 random bytes hex-encoded");
        // Second resolve reuses the persisted token.
        let t2 = resolve_token(None, dir.path());
        assert_eq!(t1, t2);
        // Env override wins and does not touch the file.
        let t3 = resolve_token(Some("envtok"), dir.path());
        assert_eq!(t3, "envtok");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("api_token"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn http_middleware_gates_api_only() {
        use axum::{middleware::from_fn_with_state, routing::get, Router};
        use tower::ServiceExt;

        let auth = Arc::new(auth_on("secret"));
        let app = Router::new()
            .route("/api/data", get(|| async { "data" }))
            .route("/api/auth/status", get(|| async { "status" }))
            .route("/", get(|| async { "shell" }))
            .layer(from_fn_with_state(auth, http_auth_mw));

        // No ConnectInfo in oneshot ⇒ treated as remote.
        let res = app.clone().oneshot(req("/api/data")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .clone()
            .oneshot(req_with_header("/api/data", "x-senclaw-token", "secret"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Handshake endpoints and the SPA shell stay open.
        let res = app.clone().oneshot(req("/api/auth/status")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app.clone().oneshot(req("/")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ws_middleware_gates_every_path() {
        use axum::{middleware::from_fn_with_state, routing::get, Router};
        use tower::ServiceExt;

        let auth = Arc::new(auth_on("secret"));
        let app = Router::new()
            .route("/", get(|| async { "ws" }))
            .route("/browser", get(|| async { "ext" }))
            .layer(from_fn_with_state(auth, ws_auth_mw));

        let res = app.clone().oneshot(req("/")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = app.clone().oneshot(req("/browser")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = app
            .clone()
            .oneshot(req("/?token=secret"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// Real listener + real client socket: proves the `ConnectInfo` plumbing
    /// (`into_make_service_with_connect_info`) actually delivers the peer
    /// address, so a loopback client is exempt even with auth required.
    #[tokio::test]
    async fn real_socket_loopback_exempt() {
        use axum::{middleware::from_fn_with_state, routing::get, Router};

        let auth = Arc::new(auth_on("secret"));
        let app = Router::new()
            .route("/api/data", get(|| async { "d" }))
            .layer(from_fn_with_state(auth, http_auth_mw));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let res = reqwest::get(format!("http://{addr}/api/data")).await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK, "loopback peer must not need a token");
    }

    #[test]
    fn mode_parsing_never_falls_back_to_off() {
        assert_eq!(AuthMode::parse_opt("always"), Some(AuthMode::Always));
        assert_eq!(AuthMode::parse_opt(" OFF "), Some(AuthMode::Off));
        assert_eq!(AuthMode::parse_opt("auto"), Some(AuthMode::Auto));
        assert_eq!(AuthMode::parse_opt("alwyas"), None);
        // A typo in the environment must land on `auto`, never on `off`.
        assert_eq!(AuthMode::from_env_value("alwyas"), AuthMode::Auto);
        assert_eq!(AuthMode::from_env_value(""), AuthMode::Auto);
        assert_eq!(AuthMode::from_env_value("off"), AuthMode::Off);
    }

    #[test]
    fn auto_mode_keys_off_the_bind_host() {
        let mut a = auth_on("secret");
        assert!(a.required(), "non-loopback bind ⇒ token required");
        a.bind_is_loopback = true;
        assert!(!a.required(), "loopback bind ⇒ no token");
    }

    #[test]
    fn always_mode_gates_loopback_too() {
        let auth = auth_always("secret");
        assert!(auth.required());
        // This is the whole point: a same-host reverse proxy makes every
        // remote client look like 127.0.0.1.
        assert!(!authorize(&auth, local_peer(), &req("/api/llm-config")));
        assert!(!authorize(&auth, remote_peer(), &req("/api/llm-config")));
        let ok = req_with_header("/api/llm-config", "x-senclaw-token", "secret");
        assert!(authorize(&auth, local_peer(), &ok));
    }

    #[test]
    fn off_mode_never_gates_even_when_exposed() {
        let mut auth = auth_on("secret");
        auth.env_mode = AuthMode::Off;
        auth.env_set = true;
        assert!(!auth.required());
        assert!(authorize(&auth, remote_peer(), &req("/api/llm-config")));
    }

    #[test]
    fn ui_override_wins_over_env_and_is_cached() {
        let cfg = crate::config::Config::from_env();
        let db = Arc::new(crate::db::Db::open_in_memory(&cfg).expect("db"));
        mode_cache_clear();
        let auth = ApiAuth {
            env_mode: AuthMode::Auto,
            env_set: true,
            bind_is_loopback: true,
            token: Some("secret".into()),
            token_path: None,
            cookie_secure: None,
            db: Some(Arc::clone(&db)),
        };
        assert_eq!(auth.effective_mode(), (AuthMode::Auto, ModeSource::Env));
        assert!(!auth.required());

        set_mode_override(&db, Some(AuthMode::Always)).unwrap();
        assert_eq!(auth.effective_mode(), (AuthMode::Always, ModeSource::Ui));
        assert!(auth.required());
        assert!(!authorize(&auth, local_peer(), &req("/api/x")));

        // Clearing hands the decision back to the environment.
        set_mode_override(&db, None).unwrap();
        assert_eq!(auth.effective_mode(), (AuthMode::Auto, ModeSource::Env));
        mode_cache_clear();
    }

    #[test]
    fn forwarded_proto_detection() {
        let mut h = HeaderMap::new();
        assert!(!forwarded_proto_is_https(&h));
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(forwarded_proto_is_https(&h));
        // A chained proxy list: the client leg is the first entry.
        h.insert("x-forwarded-proto", "https, http".parse().unwrap());
        assert!(forwarded_proto_is_https(&h));
        h.insert("x-forwarded-proto", "http".parse().unwrap());
        assert!(!forwarded_proto_is_https(&h));
    }

    #[tokio::test]
    async fn login_cookie_is_secure_only_behind_tls() {
        use axum::{routing::post, Router};
        use tower::ServiceExt;

        let call = |auth: ApiAuth, proto: Option<&str>| {
            let app = Router::new()
                .route("/api/auth/login", post(auth_login))
                .with_state(Arc::new(auth));
            let mut b = Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json");
            if let Some(p) = proto {
                b = b.header("x-forwarded-proto", p);
            }
            app.oneshot(b.body(Body::from("{\"token\":\"secret\"}")).unwrap())
        };

        // Plain LAN HTTP: no `Secure`, or the browser drops the cookie.
        let res = call(auth_on("secret"), None).await.unwrap();
        let c = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(!c.contains("Secure"), "got {c}");

        // Behind a TLS terminator: `Secure`.
        let res = call(auth_on("secret"), Some("https")).await.unwrap();
        let c = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(c.contains("Secure"), "got {c}");

        // Explicit override beats the header in both directions.
        let mut forced = auth_on("secret");
        forced.cookie_secure = Some(true);
        let res = call(forced, None).await.unwrap();
        let c = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(c.contains("Secure"), "got {c}");
    }

    #[tokio::test]
    async fn login_mints_cookie() {
        use axum::{routing::post, Router};
        use tower::ServiceExt;

        let auth = Arc::new(auth_on("secret"));
        let app = Router::new()
            .route("/api/auth/login", post(auth_login))
            .with_state(auth);

        let body = |tok: &str| {
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(format!("{{\"token\":\"{tok}\"}}")))
                .unwrap()
        };

        let res = app.clone().oneshot(body("secret")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let cookie = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(cookie.starts_with("senclaw_token=secret"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));

        let res = app.clone().oneshot(body("wrong")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res.headers().get(header::SET_COOKIE).is_none());
    }
}

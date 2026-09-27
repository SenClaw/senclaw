//! Closing a Space App's own API to everything except the SenClaw daemon.
//!
//! A Space App authenticates nothing of its own. It listens on a loopback port,
//! and every process on the machine can reach that port: its REST API, its MCP
//! endpoint, its database-backed tools. Binding loopback keeps the LAN out; it
//! does nothing about the browser extension, the other Space App, or the script
//! that happens to know the port.
//!
//! The daemon mints one access token per installed app and stamps it on every
//! request it forwards — the UI iframe, the app's own `fetch`es, MCP tool calls
//! — so an app can require it and become reachable only through the daemon.
//!
//! ```ignore
//! use app_space_sdk::auth;
//!
//! let app = Router::new()
//!     .route("/api/notes", get(list_notes))
//!     .layer(axum::middleware::from_fn(auth::require_app_token));
//! ```
//!
//! Two things are deliberately not refused:
//!
//! - **No token in the environment.** That is a bare `cargo run` outside
//!   SenClaw, and 401ing every request — including the daemon's health check —
//!   would turn "no token issued" into "app permanently down".
//! - **Exempt paths** ([`require_app_token_with`]). Pass the health path and
//!   anything a client dials directly, such as a browser extension's WebSocket.
//!   The health check decides whether the app started, and it runs before
//!   anything is ever proxied.

use axum::{
    Json,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::bridge::{HEADER_APP_TOKEN, app_token_from_env};

/// Constant-time compare — `==` on a secret short-circuits at the first
/// differing byte and leaks the matched prefix length through timing.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// The token a request presents, from the header or `?app_token=`.
fn presented(req: &Request) -> Option<String> {
    if let Some(v) = req
        .headers()
        .get(HEADER_APP_TOKEN)
        .and_then(|v| v.to_str().ok())
    {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    for pair in req.uri().query().unwrap_or("").split('&') {
        let mut it = pair.splitn(2, '=');
        if it.next() == Some("app_token") {
            let v = it.next().unwrap_or("").trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "this app only answers requests from the SenClaw daemon",
            "code": "app_token_required",
        })),
    )
        .into_response()
}

/// Whether this request may proceed, given the app's token and its exempt
/// paths. Split out from the middleware so it is testable without a server.
pub fn authorized(req: &Request, token: Option<&str>, skip: &[&str]) -> bool {
    let Some(token) = token else {
        // No token was issued — the app is not running under SenClaw.
        return true;
    };
    let path = req.uri().path();
    for pattern in skip {
        if pattern.is_empty() {
            continue;
        }
        if let Some(prefix) = pattern.strip_suffix('*') {
            if path.starts_with(prefix) {
                return true;
            }
        } else if path == *pattern {
            return true;
        }
    }
    presented(req).map(|p| ct_eq(&p, token)).unwrap_or(false)
}

/// Middleware refusing anything that does not carry this app's access token.
///
/// Exempts `/health` only. An app whose health path is elsewhere, or that has
/// endpoints a client dials directly, wants [`require_app_token_with`].
pub async fn require_app_token(req: Request, next: Next) -> Response {
    if authorized(&req, app_token_from_env().as_deref(), &["/health"]) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

/// [`require_app_token`] with the app's own exempt paths — exact matches, or a
/// trailing `*` for a prefix (`"/public/*"`).
///
/// ```ignore
/// .layer(axum::middleware::from_fn(
///     app_space_sdk::auth::require_app_token_with(&["/api/status", "/ws/*"]),
/// ))
/// ```
pub fn require_app_token_with(
    skip: &'static [&'static str],
) -> impl Fn(Request, Next) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>
+ Clone {
    move |req: Request, next: Next| {
        Box::pin(async move {
            if authorized(&req, app_token_from_env().as_deref(), skip) {
                next.run(req).await
            } else {
                unauthorized()
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    const TOKEN: &str = "sca_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn req(uri: &str, token: Option<&str>) -> Request {
        let mut b = Request::builder().uri(uri);
        if let Some(t) = token {
            b = b.header(HEADER_APP_TOKEN, t);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn only_the_daemons_own_request_passes() {
        assert!(authorized(
            &req("/api/notes", Some(TOKEN)),
            Some(TOKEN),
            &[]
        ));
        // What the guard exists to stop: another local process on the port.
        assert!(!authorized(&req("/api/notes", None), Some(TOKEN), &[]));
        assert!(!authorized(
            &req("/api/notes", Some("sca_wrong")),
            Some(TOKEN),
            &[]
        ));
    }

    #[test]
    fn exempt_paths_match_exactly_or_by_prefix() {
        let skip = ["/health", "/public/*"];
        assert!(authorized(&req("/health", None), Some(TOKEN), &skip));
        assert!(authorized(
            &req("/public/logo.png", None),
            Some(TOKEN),
            &skip
        ));
        // A prefix must not leak into a sibling path.
        assert!(!authorized(&req("/publicity", None), Some(TOKEN), &skip));
    }

    #[test]
    fn the_query_carrier_works_for_clients_that_cannot_set_headers() {
        let uri = format!("/api/notes?app_token={TOKEN}");
        assert!(authorized(&req(&uri, None), Some(TOKEN), &[]));
    }

    #[test]
    fn without_an_issued_token_the_guard_is_inert() {
        // A bare `cargo run`. Refusing everything would turn "not launched by
        // SenClaw" into "app is down".
        assert!(authorized(&req("/api/notes", None), None, &[]));
    }
}

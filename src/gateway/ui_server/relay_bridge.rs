//! Relay → HTTP API bridge.
//!
//! The Flutter mobile app reaches the daemon only through the encrypted relay
//! hub, so it cannot call the `/api/*` HTTP endpoints directly (those bind to
//! `127.0.0.1`). This bridge lets the app tunnel REST calls over the relay
//! control channel: the app sends an `API_REQ` control frame carrying
//! `{requestId, method, path, body}`, the daemon replays it through the very
//! same axum router that serves the web UI via `tower`'s `oneshot`, and sends
//! the result back as an `API_RESP` control frame.
//!
//! This reuses every existing handler (code, space, cowork, …) with zero
//! duplication — the router is the single source of truth.
//!
//! NOTE: control-frame metadata travels in plaintext (TLS to the hub, but not
//! E2E-encrypted), matching the existing AGENT_LIST / HISTORY control frames.
//! Tunnelling responses that include file contents or emails should eventually
//! move to E2E-encrypted envelopes — see the migration notes.

use std::sync::{Arc, OnceLock};

use axum::body::{to_bytes, Body};
use axum::http::{header::CONTENT_TYPE, Request};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use tower::ServiceExt;

use super::core::{build_router, UiState};

/// Max response body tunnelled over the relay (32 MiB).
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// A request decoded from an `API_REQ` control frame's `metadata`.
#[derive(Debug, Deserialize)]
pub struct ApiRequest {
    #[serde(rename = "requestId")]
    pub request_id: String,
    #[serde(default = "default_method")]
    pub method: String,
    /// Path plus optional query string, e.g. `/api/code/sessions?status=active`.
    pub path: String,
    /// JSON body as a string (omitted / null for bodyless requests).
    #[serde(default)]
    pub body: Option<String>,
}

fn default_method() -> String {
    "GET".to_string()
}

/// The payload serialised into an `API_RESP` control frame's `metadata`.
#[derive(Debug, Serialize)]
pub struct ApiResponse {
    #[serde(rename = "requestId")]
    pub request_id: String,
    pub status: u16,
    /// Response body. UTF-8 text verbatim, or base64 of the raw bytes when
    /// `body_base64` is set (binary assets — images/fonts/wasm — would be
    /// corrupted by a lossy UTF-8 conversion, so they ride as base64).
    pub body: String,
    /// The response's `Content-Type`, so a webview can serve assets correctly.
    #[serde(rename = "contentType", skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// True when `body` is base64-encoded binary rather than UTF-8 text.
    #[serde(rename = "bodyBase64", default, skip_serializing_if = "is_false")]
    pub body_base64: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl ApiResponse {
    /// Build an error response with a JSON text body.
    fn error(request_id: String, status: u16, body: String) -> Self {
        ApiResponse {
            request_id,
            status,
            body,
            content_type: Some("application/json".to_string()),
            body_base64: false,
        }
    }
}

/// Lazily-populated handle to the UI server state.
///
/// Created early during boot (before channels connect) and filled in once
/// `UiState` is constructed. The relay control handler clones this and resolves
/// the state on demand; requests that arrive before the state is ready get a
/// 503 so the client can retry.
#[derive(Default)]
pub struct ApiBridgeState {
    cell: OnceLock<Arc<UiState>>,
}

impl ApiBridgeState {
    pub fn new() -> Self {
        Self {
            cell: OnceLock::new(),
        }
    }

    /// Install the UI state. Idempotent — only the first call wins.
    pub fn set(&self, state: Arc<UiState>) {
        let _ = self.cell.set(state);
    }

    pub fn get(&self) -> Option<Arc<UiState>> {
        self.cell.get().cloned()
    }
}

/// Dispatch a single decoded API request through the UI router.
///
/// Returns the response ready to be serialised into an `API_RESP` frame.
pub async fn dispatch(bridge: &ApiBridgeState, req: ApiRequest) -> ApiResponse {
    let request_id = req.request_id.clone();

    let Some(state) = bridge.get() else {
        return ApiResponse::error(
            request_id,
            503,
            r#"{"error":"daemon not ready"}"#.to_string(),
        );
    };

    let router = build_router(state);

    let http_req = match Request::builder()
        .method(req.method.as_str())
        .uri(req.path.as_str())
        .header("content-type", "application/json")
        // A relay frame is already authenticated by relay pairing, and this
        // synthesised request carries none of the marks a browser leaves — so
        // the per-app token middleware would take the phone app for an
        // unidentified process and, under strict mode, refuse it every
        // `/api/space/apps/<id>/…` call. An extension, not a header: nothing on
        // the network can forge one.
        .extension(super::app_auth::TrustedOperator)
        .body(req.body.map(Body::from).unwrap_or_else(Body::empty))
    {
        Ok(r) => r,
        Err(e) => {
            return ApiResponse::error(
                request_id,
                400,
                format!(r#"{{"error":"bad request: {e}"}}"#),
            );
        }
    };

    match router.oneshot(http_req).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let content_type = resp
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let bytes = to_bytes(resp.into_body(), MAX_BODY_BYTES)
                .await
                .unwrap_or_default();
            // Text rides verbatim (JSON, HTML, JS, CSS); binary (images, fonts,
            // wasm) would be mangled by a lossy UTF-8 cast, so base64 it.
            match String::from_utf8(bytes.to_vec()) {
                Ok(text) => ApiResponse {
                    request_id,
                    status,
                    body: text,
                    content_type,
                    body_base64: false,
                },
                Err(_) => ApiResponse {
                    request_id,
                    status,
                    body: STANDARD.encode(&bytes),
                    content_type,
                    body_base64: true,
                },
            }
        }
        Err(_) => ApiResponse::error(
            request_id,
            500,
            r#"{"error":"router dispatch failed"}"#.to_string(),
        ),
    }
}

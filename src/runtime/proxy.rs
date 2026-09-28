//! The legacy-namespace reverse proxy (`docs/runtime-protocol.md` §5.2) and
//! the local-model route (§5.4).
//!
//! `/api/ocr/*` -> `ocr`, `/api/tts/*` -> `tts`, `/api/whisper/*` -> `asr`,
//! `/api/decision/*` -> `decision` (except the control-plane routes the
//! daemon keeps, which are registered as their own static routes and win over
//! this wildcard — axum/matchit prefers a static match over a catch-all).
//! Every request is forwarded with its method, path, query and body streamed
//! both ways, with no total timeout — a local model may legitimately take
//! minutes to finish a long completion.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use sen_runtime_sdk::manifest::Slot;
use serde_json::json;

use super::manager::{Dial, RuntimeClientError, RuntimeManager};

/// Headers stripped from what the daemon forwards in either direction: the
/// caller's own credential (replaced with the runtime's per-launch token),
/// framing headers reqwest/axum recompute themselves, and the RFC 7230
/// hop-by-hop set — meaningful only between a client and its *immediate*
/// next hop, so forwarding them to the runtime as if they applied end-to-end
/// is incorrect proxy behavior even though the loopback runtime accepting
/// them is low-risk on its own.
const STRIP_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "x-senclaw-token",
    "cookie",
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];
const STRIP_RESPONSE_HEADERS: &[&str] = &["connection", "transfer-encoding", "content-length"];

/// The §5.2 structured error body (`code`, `slot`, `error`) — shared by the
/// legacy-namespace proxy and every other place a runtime failing to
/// install/select/start must answer the same way a caller already knows how
/// to handle (`POST /api/local-models/:key/load`, `POST
/// /api/runtimes/slots/:slot/start`).
pub(crate) fn error_response(err: RuntimeClientError, slot: Slot) -> Response {
    let status = match &err {
        RuntimeClientError::NotInstalled { .. } | RuntimeClientError::NotSelected { .. } => StatusCode::SERVICE_UNAVAILABLE,
        RuntimeClientError::StartFailed { .. } => StatusCode::SERVICE_UNAVAILABLE,
        RuntimeClientError::Upstream(_) => StatusCode::BAD_GATEWAY,
        RuntimeClientError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = json!({ "error": err.to_string(), "code": err.code(), "slot": slot.as_str() });
    (status, axum::Json(body)).into_response()
}

/// Ensure the slot's runtime is running, then forward `req` to it verbatim
/// (same method + path + query, streamed body both ways).
async fn forward(manager: &Arc<RuntimeManager>, slot: Slot, req: Request) -> Response {
    let dial = match manager.ensure_slot_started(slot).await {
        Ok(d) => d,
        Err(e) => return error_response(e, slot),
    };
    manager.begin_request(&dial.process_key);
    match relay(&dial, req, Arc::clone(manager)).await {
        // `in_flight` stays elevated until the streamed body itself is
        // dropped — see `relay`'s `EndOnDrop` — so no `end_request`
        // here on success.
        Ok(r) => r,
        Err(e) => {
            manager.end_request(&dial.process_key);
            // §3.2 step 9: a relay failure may mean the runtime
            // actually crashed. Evict it if so, so the *next* request
            // respawns instead of dialing a dead port for the rest of the
            // idle timeout.
            manager.evict_if_crashed(&dial.process_key);
            error_response(RuntimeClientError::Upstream(e), slot)
        }
    }
}

/// Defers `RuntimeManager::end_request` until the streamed response body
/// itself is dropped — reaching its end normally, or the client disconnecting
/// early — rather than firing it right after `relay` returns with only the
/// headers. Without this the idle sweep can see `in_flight == 0` while a long
/// generation is still streaming and stop the process mid-stream (see
/// CLAUDE.md: "the sweep skips any process with a nonzero count").
struct EndOnDrop {
    manager: Arc<RuntimeManager>,
    key: String,
}

impl Drop for EndOnDrop {
    fn drop(&mut self) {
        self.manager.end_request(&self.key);
    }
}

/// Send `req` to `dial` and stream its answer back, whatever path suffix and
/// method it used — this is what makes the daemon a transparent passthrough
/// for a namespace whose exact route list it does not need to know.
///
/// The caller must already have called `manager.begin_request`. On `Ok`,
/// responsibility for the matching `end_request` moves to the returned
/// response body (`EndOnDrop`, dropped when the body is); on `Err` no body was
/// ever created, so the caller's own `end_request` is what covers it.
async fn relay(dial: &Dial, req: Request, manager: Arc<RuntimeManager>) -> Result<Response, String> {
    let method = req.method().clone();
    let path_and_query = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
    let headers = req.headers().clone();
    let body = req.into_body();
    let body_bytes = axum::body::to_bytes(body, 512 * 1024 * 1024)
        .await
        .map_err(|e| format!("could not read the request body: {e}"))?;

    let url = format!("{}{path_and_query}", dial.base_url);
    let client = reqwest::Client::builder()
        .read_timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let reqwest_method =
        reqwest::Method::from_bytes(method.as_str().as_bytes()).map_err(|_| format!("unsupported method {method}"))?;
    let mut builder = client.request(reqwest_method, &url).bearer_auth(&dial.token);
    for (name, value) in headers.iter() {
        if STRIP_REQUEST_HEADERS.contains(&name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    if !body_bytes.is_empty() {
        builder = builder.body(body_bytes.to_vec());
    }

    let upstream = builder.send().await.map_err(|e| format!("could not reach the runtime: {e}"))?;
    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut out_headers = HeaderMap::new();
    for (name, value) in upstream.headers().iter() {
        if STRIP_RESPONSE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_str().as_bytes()), HeaderValue::from_bytes(value.as_bytes())) {
            out_headers.append(n, v);
        }
    }
    // `end_guard` is moved into the stream's own closure, so it lives
    // exactly as long as the stream does — dropped (and `end_request` fired)
    // only once the body is fully consumed or the client/hyper drops it
    // early, never right here at the header round-trip.
    let end_guard = EndOnDrop { manager, key: dial.process_key.clone() };
    let stream = upstream.bytes_stream().inspect(move |_| {
        let _ = &end_guard;
    });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = out_headers;
    Ok(response)
}

macro_rules! slot_proxy_handler {
    ($name:ident, $slot:expr) => {
        pub async fn $name(State(s): State<std::sync::Arc<crate::gateway::ui_server::core::UiState>>, req: Request) -> Response {
            let Some(manager) = s.runtime_manager.as_ref() else {
                return error_response(RuntimeClientError::Internal("the runtime manager is not wired".into()), $slot);
            };
            forward(manager, $slot, req).await
        }
    };
}

slot_proxy_handler!(proxy_ocr, Slot::Ocr);
slot_proxy_handler!(proxy_tts, Slot::Tts);
slot_proxy_handler!(proxy_whisper, Slot::Asr);
slot_proxy_handler!(proxy_decision, Slot::Decision);
slot_proxy_handler!(proxy_browser, Slot::Browser);

// ===== Decision settings: merge daemon-owned gate/skills into the proxied body =====

use axum::Json as AxumJson;

use crate::gateway::group_manager::load_decision_settings;
use crate::gateway::ui_server::core::{AppError, UiState};

fn merge_control_plane(value: &mut serde_json::Value, config_path: &std::path::Path) {
    let control = load_decision_settings(config_path);
    let gate = serde_json::to_value(&control.gate).unwrap_or_default();
    let skills = serde_json::to_value(&control.skills).unwrap_or_default();
    if let Some(settings_obj) = value.get_mut("settings").and_then(|v| v.as_object_mut()) {
        settings_obj.insert("gate".to_string(), gate);
        settings_obj.insert("skills".to_string(), skills);
    } else if let Some(obj) = value.as_object_mut() {
        obj.insert("gate".to_string(), gate);
        obj.insert("skills".to_string(), skills);
    }
}

async fn call_decision(manager: &RuntimeManager, method: Method, body: Option<Vec<u8>>) -> Result<serde_json::Value, Response> {
    let dial = match manager.ensure_slot_started(Slot::Decision).await {
        Ok(d) => d,
        Err(e) => return Err(error_response(e, Slot::Decision)),
    };
    manager.begin_request(&dial.process_key);
    let client = reqwest::Client::new();
    let url = format!("{}/api/decision/settings", dial.base_url);
    let mut req = client.request(method, &url).bearer_auth(&dial.token);
    if let Some(b) = &body {
        req = req.header(reqwest::header::CONTENT_TYPE, "application/json").body(b.clone());
    }
    let result = req.send().await;
    manager.end_request(&dial.process_key);
    let resp = result.map_err(|e| error_response(RuntimeClientError::Upstream(e.to_string()), Slot::Decision))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // The runtime's *own* error — its status and body pass through
        // unchanged, never rewrapped into the daemon's 502 `runtime_error`
        // shape (`docs/runtime-protocol.md` §5.2: "the decision-settings
        // merge only touches a 2xx body").
        let out_status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        return Err((out_status, [(axum::http::header::CONTENT_TYPE, "application/json")], text).into_response());
    }
    serde_json::from_str(&text)
        .map_err(|e| error_response(RuntimeClientError::Internal(format!("unreadable settings answer: {e}")), Slot::Decision))
}

/// `GET /api/decision/settings` — proxied, with the daemon's own `gate`/
/// `skills` merged into `settings` so existing clients render unchanged.
pub(crate) async fn decision_settings_get(State(s): State<std::sync::Arc<UiState>>) -> Response {
    let Some(manager) = s.runtime_manager.as_ref() else {
        return error_response(RuntimeClientError::Internal("the runtime manager is not wired".into()), Slot::Decision);
    };
    match call_decision(manager, Method::GET, None).await {
        Ok(mut value) => {
            merge_control_plane(&mut value, &s.config.paths.global_config_path);
            AxumJson(value).into_response()
        }
        Err(resp) => resp,
    }
}

/// `PUT /api/decision/settings` — forwards only `backend`/`local`/`online` and
/// keeps the daemon's own stored gate/skills untouched (they have their own
/// endpoints: `PUT /api/decision/gate`, `PUT /api/decision/skills`).
pub(crate) async fn decision_settings_put(State(s): State<std::sync::Arc<UiState>>, body: axum::body::Bytes) -> Response {
    let Some(manager) = s.runtime_manager.as_ref() else {
        return error_response(RuntimeClientError::Internal("the runtime manager is not wired".into()), Slot::Decision);
    };
    let mut value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return AppError(StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")).into_response(),
    };
    if let Some(obj) = value.as_object_mut() {
        obj.remove("gate");
        obj.remove("skills");
    }
    let forward_body = match serde_json::to_vec(&value) {
        Ok(b) => b,
        Err(e) => return AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match call_decision(manager, Method::PUT, Some(forward_body)).await {
        Ok(mut value) => {
            merge_control_plane(&mut value, &s.config.paths.global_config_path);
            AxumJson(value).into_response()
        }
        Err(resp) => resp,
    }
}

// ===== Local models as an OpenAI-compatible provider (§5.4) =====

/// `/api/runtimes/models/:key/*` — load `key` on demand (JIT, like a session
/// Space App) and proxy to its process. `rest` is the tail after the key
/// (`v1/chat/completions`, `v1/embeddings`, …).
pub(crate) async fn proxy_model(
    State(s): State<std::sync::Arc<UiState>>,
    axum::extract::Path((key, _rest)): axum::extract::Path<(String, String)>,
    req: Request,
) -> Response {
    let Some(manager) = s.runtime_manager.as_ref() else {
        return AppError(StatusCode::INTERNAL_SERVER_ERROR, "the runtime manager is not wired".into()).into_response();
    };
    let Some(model) = crate::local_models::scan::find_by_key(&s.config.paths.local_models_dir, &key) else {
        return AppError(StatusCode::NOT_FOUND, format!("no local model with key `{key}`")).into_response();
    };
    let capabilities = model.capability_list();
    // The OpenAI-compatible route has no per-request way to name a context
    // length (unlike `POST /api/local-models/:key/load {contextLength}`), so
    // this always resolves to the daemon's effective cap (§5.3): never the
    // model's full maximum by default.
    let default_context_length = crate::local_models::settings::load_daemon_settings(&s.config.paths.local_models_dir).default_context_length;
    let context_length = crate::local_models::settings::resolve_context_length(None, model.context_length, default_context_length);
    let dial = match manager
        .ensure_model_started(model.format, &model.key, &model.path, model.mmproj_path.as_deref(), context_length, capabilities)
        .await
    {
        Ok(d) => d,
        Err(e) => return error_response(e, sen_runtime_sdk::manifest::Slot::for_format(model.format)),
    };
    manager.begin_request(&dial.process_key);
    // Rewrite the incoming path (`/api/runtimes/models/<key>/v1/...`) onto the
    // model process's own root (`/v1/...`) before relaying.
    let (mut parts, body) = req.into_parts();
    let prefix = format!("/api/runtimes/models/{key}");
    let new_path = parts.uri.path().strip_prefix(&prefix).unwrap_or("/");
    let new_pq = match parts.uri.query() {
        Some(q) => format!("{new_path}?{q}"),
        None => new_path.to_string(),
    };
    parts.uri = match Uri::builder().path_and_query(new_pq).build() {
        Ok(u) => u,
        Err(e) => {
            manager.end_request(&dial.process_key);
            return AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    let rewritten = Request::from_parts(parts, body);
    match relay(&dial, rewritten, Arc::clone(manager)).await {
        // `in_flight` stays elevated until the streamed body itself is
        // dropped — no `end_request` here on success.
        Ok(r) => r,
        Err(e) => {
            manager.end_request(&dial.process_key);
            // A relay failure may mean the model process crashed —
            // evict it so the next call respawns instead of dialing a dead
            // port until the idle timeout notices.
            manager.evict_if_crashed(&dial.process_key);
            error_response(RuntimeClientError::Upstream(e), sen_runtime_sdk::manifest::Slot::for_format(model.format))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_match_the_protocol_taxonomy() {
        assert_eq!(RuntimeClientError::NotInstalled { slot: "OCR".into() }.code(), "runtime_not_installed");
        assert_eq!(RuntimeClientError::NotSelected { slot: "OCR".into() }.code(), "runtime_not_selected");
        assert_eq!(
            RuntimeClientError::StartFailed { slot: "OCR".into(), detail: "x".into() }.code(),
            "runtime_start_failed"
        );
    }

    #[test]
    fn strip_lists_cover_the_credential_and_framing_headers() {
        assert!(STRIP_REQUEST_HEADERS.contains(&"authorization"));
        assert!(STRIP_REQUEST_HEADERS.contains(&"x-senclaw-token"));
        assert!(STRIP_RESPONSE_HEADERS.contains(&"transfer-encoding"));
    }

    /// The full RFC 7230 §6.1 hop-by-hop set must never reach the
    /// runtime — it applies only between a client and its immediate next hop.
    #[test]
    fn strip_request_headers_cover_the_rfc7230_hop_by_hop_set() {
        for h in ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade"] {
            assert!(STRIP_REQUEST_HEADERS.contains(&h), "`{h}` must be stripped from the forwarded request");
        }
    }

    fn test_manager(tmp: &std::path::Path) -> Arc<RuntimeManager> {
        RuntimeManager::new(crate::runtime::manager::RuntimeManagerConfig {
            runtimes_dir: tmp.join("runtimes"),
            runtime_data_dir: tmp.join("runtime-data"),
            runtime_logs_dir: tmp.join("logs"),
            bundled_dir: None,
            local_models_dir: tmp.join("local-models"),
            config_path: tmp.join("config.json"),
            home: tmp.to_path_buf(),
            index_url: "file:///dev/null".to_string(),
        })
    }

    /// `relay` must keep `in_flight` elevated for as long as the response
    /// body is being streamed, not just for the header round-trip — dropping
    /// it only once the body is fully consumed (or dropped early).
    #[tokio::test]
    async fn in_flight_stays_elevated_until_the_streamed_body_is_fully_consumed() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = test_manager(tmp.path());
        let key = "service:slow-upstream";
        let proc = manager.track_process_for_test(key);

        // A tiny upstream that answers with a two-chunk streamed body, with a
        // real delay before the second chunk — long enough that asserting
        // "still in flight" right after `relay` returns is never racy.
        let app = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                let first = futures::stream::once(async { Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"a")) });
                let second = futures::stream::once(async {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"b"))
                });
                axum::body::Body::from_stream(first.chain(second))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dial = Dial { base_url: format!("http://{addr}"), token: String::new(), process_key: key.to_string() };
        manager.begin_request(key);
        assert_eq!(proc.in_flight.load(std::sync::atomic::Ordering::Relaxed), 1);

        let req = Request::builder().method("GET").uri("/slow").body(Body::empty()).unwrap();
        let resp = relay(&dial, req, Arc::clone(&manager)).await.expect("the upstream must answer");

        // Headers are back, but the guard travels with the body, not with
        // this return — the sweep must still see this process as in use.
        assert_eq!(
            proc.in_flight.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "in_flight must survive the header round-trip, before the body is read"
        );

        // Fully drain the body, exactly as hyper/axum would while proxying it
        // to a real client — only once that finishes does the guard fire.
        let _ = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let mut dropped = false;
        for _ in 0..100 {
            if proc.in_flight.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                dropped = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(dropped, "in_flight must drop back to 0 once the stream is fully consumed");
    }
}

//! {{title_name}} — a SenClaw Space App.
//!
//! What the daemon does with this, in order:
//!
//! 1. Reads `senclaw-manifest.json`. `runtime.mode` is `session`, so nothing
//!    starts at boot.
//! 2. Starts this process when the user opens the app, or when an agent calls
//!    one of the MCP tools in [`mcp`], and waits for `runtime.healthPath`
//!    (`/api/status`) to answer before calling it started.
//! 3. Stops it again 60 seconds after the last request
//!    (`runtime.idleTimeoutSecs`).
//!
//! The tools stay in every agent's roster while this is stopped: the tool list
//! is cached and the MCP URL points at the daemon's proxy, which starts the app
//! before forwarding the call.
//!
//! Run it by hand during development:
//!
//!     SENCLAW_SPACE_APP_ID={{id}} PORT={{port}} cargo run

mod mcp;
mod space;

use std::sync::Arc;

use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tower_http::services::ServeDir;

pub struct AppState {
    pub started: std::time::Instant,
    pub space: space::Space,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or({{port}});

    let state = Arc::new(AppState {
        started: std::time::Instant::now(),
        space: space::Space::from_env("{{id}}"),
    });

    let api = Router::new()
        // runtime.healthPath. The daemon waits on this before it calls the app
        // started and polls it afterwards, so it must stay cheap and never
        // block on anything.
        .route("/status", get(status))
        .route("/visit", post(visit))
        // MCP over HTTP: the daemon POSTs JSON-RPC here. The GET is the SSE
        // half of the transport, which the client opens but this app never
        // needs to push on.
        .route("/mcp/sse", post(mcp::message).get(mcp::sse))
        .with_state(state.clone());

    // No CORS layer, deliberately. The UI is served from this same origin (the
    // daemon proxies both), so nothing here needs cross-origin access — while a
    // permissive layer would let any website the user visits POST to
    // /api/mcp/sse on loopback and read the answer. A Space App authenticates
    // nothing of its own, so same-origin is the only thing guarding it.
    let app = Router::new()
        .nest("/api", api)
        .fallback_service(ServeDir::new(web_dir()).append_index_html_on_directories(true));

    // Loopback by default. A Space App authenticates nothing of its own — the
    // daemon reaches it over 127.0.0.1 and the UI is same-origin — so binding
    // 0.0.0.0 hands the whole REST + MCP surface to anyone on the LAN. Set
    // SENCLAW_BIND_HOST=0.0.0.0 to opt in to that explicitly.
    let host = std::env::var("SENCLAW_BIND_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    println!("[{{id}}] listening on http://{host}:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// The UI lives next to the binary once packed, and inside the project during
/// development. Both are checked so `cargo run` and the installed app behave
/// the same.
fn web_dir() -> std::path::PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    for candidate in [
        std::path::PathBuf::from("web"),
        exe_dir.join("web"),
        exe_dir.join("web_dist"),
    ] {
        if candidate.join("index.html").exists() {
            return candidate;
        }
    }
    std::path::PathBuf::from("web")
}

async fn status(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "app": "{{id}}",
        "uptimeSecs": state.started.elapsed().as_secs(),
    }))
}

/// The config KV: the same store the app's own UI reads and writes, which is
/// why settings belong there and not in a file inside the app directory that an
/// update would overwrite.
async fn visit(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse {
    let current = state
        .space
        .get_config("visits")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let next = current + 1;
    match state.space.set_config("visits", json!(next)).await {
        Ok(()) => Json(json!({ "visits": next })).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

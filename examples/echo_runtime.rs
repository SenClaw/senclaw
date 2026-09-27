//! Minimal `sen-runtime-sdk`-based runtime, used by
//! `tests/runtime_manager_lifecycle.rs` to exercise install-local ->
//! supervise -> health-gate -> proxy -> idle-sweep -> cleanup end to end
//! against a **real child process**, without needing a full `sen-*` runtime
//! checked out as a sibling repo.
//!
//! Declares itself an `ocr` service (any capability slot would do for this
//! test — `ocr` is a convenient, already-proxied one) and answers
//! `GET /api/ocr/models` so the test can reach it *through* the daemon's
//! generic legacy-namespace proxy, not just talk to its port directly.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use sen_runtime_sdk::env::LaunchEnv;
use sen_runtime_sdk::manifest::{Capability, RunMode};
use sen_runtime_sdk::server::{init_tracing, serve, Readiness, ServeArgs, ServeOptions};

#[derive(Clone)]
struct EchoState;

async fn echo_models(State(_s): State<EchoState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "echo": true, "models": [] }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    // Accept (and ignore) a leading `serve` subcommand, matching every real
    // `sen-*` runtime's CLI shape (`sen-ocr serve --host H --port P`).
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().map(String::as_str) == Some("serve") {
        raw.remove(0);
    }
    let args = ServeArgs::parse(raw).map_err(|e| anyhow::anyhow!(e))?;
    let env = LaunchEnv::from_env("echo-runtime", "0.0.1");

    let routes = Router::new().route("/api/ocr/models", get(echo_models)).with_state(EchoState);
    let opts = ServeOptions {
        env,
        mode: RunMode::Service,
        capabilities: vec![Capability::Ocr],
        readiness: Readiness::ready(),
        info_detail: None,
        args,
    };
    serve(routes, opts).await
}

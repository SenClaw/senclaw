//! The app-side of the dispatch contract: implement [`DispatchProvider`] over your
//! own data, mount [`dispatch_router`], and your Space App becomes dispatchable by
//! the core `MCPDispatcher` — no source-specific engine code required.

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use std::sync::Arc;

use super::types::{Capacity, FinalizeRequest, ItemIdRequest, Outcome, PollRequest, WorkItem};

/// Implemented by a Space App over its own store. The four methods mirror
/// [`super::DispatchSource`] but are the *server* side.
#[async_trait]
pub trait DispatchProvider: Send + Sync {
    /// Atomically claim up to `capacity` ready items and return them as work items.
    async fn claim_ready(&self, capacity: Capacity) -> anyhow::Result<Vec<WorkItem>>;
    /// Extend the lease on an in-flight item.
    async fn heartbeat(&self, item_id: &str) -> anyhow::Result<()>;
    /// Return dead-worker / expired-lease items to the ready state; return their ids.
    async fn reclaim(&self) -> anyhow::Result<Vec<String>>;
    /// Record a terminal outcome for an item.
    async fn finalize(&self, item_id: &str, outcome: Outcome) -> anyhow::Result<()>;
}

/// Build the `/poll`, `/heartbeat`, `/reclaim`, `/finalize` routes. Nest it under
/// `/api/dispatch` in your app:
///
/// ```ignore
/// let app = Router::new().nest("/api/dispatch", dispatch_router(provider));
/// ```
pub fn dispatch_router(provider: Arc<dyn DispatchProvider>) -> Router {
    Router::new()
        .route("/poll", post(poll))
        .route("/heartbeat", post(heartbeat))
        .route("/reclaim", post(reclaim))
        .route("/finalize", post(finalize))
        .with_state(provider)
}

struct Err500(String);
impl IntoResponse for Err500 {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": self.0 })),
        )
            .into_response()
    }
}

async fn poll(
    State(p): State<Arc<dyn DispatchProvider>>,
    Json(req): Json<PollRequest>,
) -> Result<Json<Vec<WorkItem>>, Err500> {
    p.claim_ready(req.capacity)
        .await
        .map(Json)
        .map_err(|e| Err500(e.to_string()))
}

async fn heartbeat(
    State(p): State<Arc<dyn DispatchProvider>>,
    Json(req): Json<ItemIdRequest>,
) -> Result<Json<serde_json::Value>, Err500> {
    p.heartbeat(&req.item_id)
        .await
        .map_err(|e| Err500(e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn reclaim(State(p): State<Arc<dyn DispatchProvider>>) -> Result<Json<Vec<String>>, Err500> {
    p.reclaim()
        .await
        .map(Json)
        .map_err(|e| Err500(e.to_string()))
}

async fn finalize(
    State(p): State<Arc<dyn DispatchProvider>>,
    Json(req): Json<FinalizeRequest>,
) -> Result<Json<serde_json::Value>, Err500> {
    p.finalize(&req.item_id, req.outcome)
        .await
        .map_err(|e| Err500(e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

//! The `DispatchSource` trait (what the engine drives) and `HttpDispatchSource`
//! (the client that adapts a remote Space App's REST dispatch contract into it).

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::time::Duration;

use super::provider::DispatchProvider;
use super::types::{Capacity, FinalizeRequest, ItemIdRequest, Outcome, PollRequest, WorkItem};

/// A source of dispatchable work the `MCPDispatcher` engine can drive. Implement
/// this in-process (Rust + a DB) or, for a remote Space App, use
/// [`HttpDispatchSource`] over the REST contract.
#[async_trait]
pub trait DispatchSource: Send + Sync {
    /// Stable id, e.g. `"kanban"` or `"kanban:board-3"`.
    fn id(&self) -> &str;

    /// Atomically claim up to `capacity` ready items (deps satisfied, under WIP +
    /// per-assignee limits). Claiming must set a lease so a crash can be reclaimed.
    async fn poll_ready(&self, capacity: Capacity) -> Result<Vec<WorkItem>>;

    /// Extend the lease on an in-flight item (called while the worker runs).
    async fn heartbeat(&self, item_id: &str) -> Result<()>;

    /// Return items whose worker died / lease expired to the ready state. Returns
    /// the reclaimed item ids.
    async fn reclaim(&self) -> Result<Vec<String>>;

    /// Record the terminal outcome of a run.
    async fn finalize(&self, item_id: &str, outcome: Outcome) -> Result<()>;
}

/// Drives a remote Space App that mounts [`super::dispatch_router`]. Adapts the
/// `/api/dispatch/*` REST contract into a [`DispatchSource`] with zero
/// source-specific code — this is how the engine reaches any HTTP source.
#[derive(Clone)]
pub struct HttpDispatchSource {
    id: String,
    base_url: String,
    http: reqwest::Client,
}

impl HttpDispatchSource {
    /// `base_url` is the app root (e.g. `http://127.0.0.1:4400`); `id` names the
    /// source in the engine's logs/registry.
    pub fn new(base_url: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/dispatch/{}", self.base_url, path)
    }
}

#[async_trait]
impl DispatchSource for HttpDispatchSource {
    fn id(&self) -> &str {
        &self.id
    }

    async fn poll_ready(&self, capacity: Capacity) -> Result<Vec<WorkItem>> {
        let items = self
            .http
            .post(self.url("poll"))
            .json(&PollRequest { capacity })
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| anyhow!("dispatch poll failed: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow!("dispatch poll status: {e}"))?
            .json::<Vec<WorkItem>>()
            .await
            .map_err(|e| anyhow!("dispatch poll parse: {e}"))?;
        Ok(items)
    }

    async fn heartbeat(&self, item_id: &str) -> Result<()> {
        self.http
            .post(self.url("heartbeat"))
            .json(&ItemIdRequest {
                item_id: item_id.to_string(),
            })
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| anyhow!("dispatch heartbeat failed: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow!("dispatch heartbeat status: {e}"))?;
        Ok(())
    }

    async fn reclaim(&self) -> Result<Vec<String>> {
        let ids = self
            .http
            .post(self.url("reclaim"))
            .json(&serde_json::json!({}))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| anyhow!("dispatch reclaim failed: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow!("dispatch reclaim status: {e}"))?
            .json::<Vec<String>>()
            .await
            .unwrap_or_default();
        Ok(ids)
    }

    async fn finalize(&self, item_id: &str, outcome: Outcome) -> Result<()> {
        self.http
            .post(self.url("finalize"))
            .json(&FinalizeRequest {
                item_id: item_id.to_string(),
                outcome,
            })
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| anyhow!("dispatch finalize failed: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow!("dispatch finalize status: {e}"))?;
        Ok(())
    }
}

/// Adapts an in-process [`DispatchProvider`] into a [`DispatchSource`] with no
/// HTTP hop — used when a source (e.g. the built-in Kanban board) lives in the
/// same process as the engine.
pub struct LocalDispatchSource {
    id: String,
    provider: std::sync::Arc<dyn DispatchProvider>,
}

impl LocalDispatchSource {
    pub fn new(id: impl Into<String>, provider: std::sync::Arc<dyn DispatchProvider>) -> Self {
        Self {
            id: id.into(),
            provider,
        }
    }
}

#[async_trait]
impl DispatchSource for LocalDispatchSource {
    fn id(&self) -> &str {
        &self.id
    }
    async fn poll_ready(&self, capacity: Capacity) -> Result<Vec<WorkItem>> {
        self.provider.claim_ready(capacity).await
    }
    async fn heartbeat(&self, item_id: &str) -> Result<()> {
        self.provider.heartbeat(item_id).await
    }
    async fn reclaim(&self) -> Result<Vec<String>> {
        self.provider.reclaim().await
    }
    async fn finalize(&self, item_id: &str, outcome: Outcome) -> Result<()> {
        self.provider.finalize(item_id, outcome).await
    }
}

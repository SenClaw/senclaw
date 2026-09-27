//! Native job registry.
//!
//! Core upkeep that already runs as ad-hoc `tokio::spawn` loops at boot —
//! cognitive decay, cognitive maintenance, the SOUL.md watcher — is invisible
//! and un-pausable today. Registering it here brings it under the same run
//! history, statistics, and pause surface as prompt tasks, while its body stays
//! Rust rather than an agent.
//!
//! **Scope limit, deliberately.** Infrastructure watchdogs that must never be
//! user-pausable (the Space-App supervisor, the MCP client watchdog, the
//! persona/memory file watchers) and tight 1.5–2 s change-detection pollers
//! (Kanban→WS) do **not** belong here — the former because pausing them breaks
//! the daemon, the latter because they run two orders of magnitude below any
//! scheduler tick. Nor do `consolidate.rs` / `reflection.rs`, which ride the
//! conversation lifecycle rather than a timer.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::RwLock;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

/// A native job: given a cancellation token, do the work and return a one-line
/// summary for the run record.
pub type NativeJobFn = std::sync::Arc<
    dyn Fn(CancellationToken) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> + Send + Sync,
>;

/// Registry of native job bodies, keyed by the `native_job` column.
///
/// Registration happens at boot, but the *task rows* are what the scheduler
/// reads — so a job whose key is registered but has no row never fires, and a
/// row whose key is missing from the registry records an honest error rather
/// than silently doing nothing.
#[derive(Default)]
pub struct NativeRegistry {
    jobs: RwLock<HashMap<String, NativeJobFn>>,
}

impl NativeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, key: &str, job: NativeJobFn) {
        self.jobs.write().unwrap().insert(key.to_owned(), job);
        tracing::debug!(key, "[background] native job registered");
    }

    pub fn get(&self, key: &str) -> Option<NativeJobFn> {
        self.jobs.read().unwrap().get(key).cloned()
    }

    pub fn keys(&self) -> Vec<String> {
        self.jobs.read().unwrap().keys().cloned().collect()
    }
}

/// Wrap an async closure into a [`NativeJobFn`].
///
/// ```ignore
/// registry.register("core.cognitive.decay", native_job(move |_cancel| {
///     let sys = sys.clone();
///     async move { sys.decay_tick().await.map(|n| format!("decayed {n} nodes")) }
/// }));
/// ```
pub fn native_job<F, Fut>(f: F) -> NativeJobFn
where
    F: Fn(CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    std::sync::Arc::new(move |cancel| Box::pin(f(cancel)))
}

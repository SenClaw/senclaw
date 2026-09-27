//! `CognitiveSystem` — single handle the daemon (and MCP servers) share.
//!
//! Bundles every dependency the cognitive layer needs into one `Arc` so
//! callers don't have to thread four pointers everywhere. This is the
//! integration boundary: the daemon constructs one of these at boot, the
//! MCP tools (P6) grab it, and the decay ticker reads from it.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::db::Db;
use crate::memory::embedding::{create_embedding_provider, EmbeddingProvider};

use super::cognify::{CognifyOptions, CognifyPipeline, CognifyReport};
use super::decay_tick::{start_decay_ticker, DecayConfig, DecayReport};
use super::embed::CognitiveEmbedder;
use super::graph_store::{GraphStore, SqliteGraphStore};
use super::llm::LlmClient;
use super::retrievers::CognitiveRetriever;
use super::search::{SearchHit, SearchQuery};
use super::vector_store::{SqliteVectorStore, VectorStore};

pub struct CognitiveSystem {
    pub graph: Arc<dyn GraphStore>,
    pub vector: Arc<dyn VectorStore>,
    pub embedder: Arc<CognitiveEmbedder>,
    pub retriever: Arc<CognitiveRetriever>,
    pipeline: CognifyPipeline,
    /// Governs how many cognify calls can run at once. Sized from
    /// `CognitiveConfig.max_concurrent` at boot. Defends against the
    /// scenario where a busy chat queues N parallel local-LLM calls that
    /// each tie up the runtime for 30 s+ — by the time the queue drains
    /// the user has long forgotten they sent any of those messages.
    cognify_semaphore: Arc<tokio::sync::Semaphore>,
    /// True when the master `enabled` switch is off. cognify() short-
    /// circuits to a no-op report so callers can keep their interfaces
    /// unchanged.
    enabled: bool,
}

impl CognitiveSystem {
    /// Construct from already-built dependencies. Tests + daemon boot use
    /// this. Defaults to a single-permit semaphore + enabled=true so
    /// existing callers behave like before — the daemon overrides via
    /// `with_config` to apply the env-driven governance knobs.
    pub fn new(
        graph: Arc<dyn GraphStore>,
        vector: Arc<dyn VectorStore>,
        provider: Arc<dyn EmbeddingProvider>,
        llm: Arc<dyn LlmClient>,
    ) -> Self {
        Self::new_with_limits(graph, vector, provider, llm, 1, true)
    }

    /// Like [`Self::new`] but lets the caller specify concurrency cap +
    /// master-enabled flag. Used by the daemon boot path which reads
    /// `CognitiveConfig` from the live `Config`.
    pub fn new_with_limits(
        graph: Arc<dyn GraphStore>,
        vector: Arc<dyn VectorStore>,
        provider: Arc<dyn EmbeddingProvider>,
        llm: Arc<dyn LlmClient>,
        max_concurrent: usize,
        enabled: bool,
    ) -> Self {
        let embedder = Arc::new(CognitiveEmbedder::new(
            Arc::clone(&graph),
            Arc::clone(&vector),
            provider,
        ));
        // The retriever and pipeline each get their own `CognitiveEmbedder`
        // clone — the inner `Arc`s are shared, so this is cheap and keeps
        // the call sites read-only.
        let retr_embed = CognitiveEmbedder::new(
            Arc::clone(&embedder.graph),
            Arc::clone(&embedder.vector),
            Arc::clone(&embedder.provider),
        );
        let pipe_embed = CognitiveEmbedder::new(
            Arc::clone(&embedder.graph),
            Arc::clone(&embedder.vector),
            Arc::clone(&embedder.provider),
        );
        let retriever = Arc::new(CognitiveRetriever::new(Arc::new(retr_embed)));
        let pipeline = CognifyPipeline::new(pipe_embed, llm);
        Self {
            graph,
            vector,
            embedder,
            retriever,
            pipeline,
            cognify_semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1))),
            enabled,
        }
    }

    /// Convenience constructor that wires the SQLite-backed stores from a
    /// shared `Db`. Callers still supply embedder + LLM.
    pub fn with_sqlite(
        db: Arc<Db>,
        provider: Arc<dyn EmbeddingProvider>,
        llm: Arc<dyn LlmClient>,
    ) -> Self {
        let graph: Arc<dyn GraphStore> = Arc::new(SqliteGraphStore::new(Arc::clone(&db)));
        let vector: Arc<dyn VectorStore> = Arc::new(SqliteVectorStore::new(db));
        Self::new(graph, vector, provider, llm)
    }

    /// Run cognify, governed by the system's enabled flag + concurrency
    /// semaphore. When disabled, returns an empty report immediately so
    /// callers don't have to plumb the flag. When at capacity, await a
    /// permit (cheap because cognify is rate-limited, not throughput-bound).
    pub async fn cognify(
        &self,
        text: &str,
        source: &str,
        opts: &CognifyOptions,
    ) -> Result<CognifyReport> {
        if !self.enabled {
            return Ok(CognifyReport::default());
        }
        let _permit = self
            .cognify_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| anyhow::anyhow!("cognify semaphore closed: {e}"))?;
        self.pipeline.cognify(text, source, opts).await
    }

    /// True when the master `cognitive.enabled` flag is on. Used by callers
    /// (e.g. reflection) that want to short-circuit BEFORE building the
    /// expensive payload, not just before acquiring a permit.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        self.retriever.search(query).await
    }

    /// Start the background decay loop. Returns the join handle so the
    /// daemon can `abort()` on shutdown.
    pub fn start_decay(&self, cfg: DecayConfig) -> JoinHandle<()> {
        start_decay_ticker(Arc::clone(&self.graph), cfg)
    }

    /// One-shot decay sweep on the calling thread — useful for tests and for
    /// pre-shutdown flush.
    pub fn decay_now(&self) -> Result<DecayReport> {
        super::decay_tick::run_decay(&*self.graph, &DecayConfig::default())
    }

    /// Quick stats for debug/status endpoints. Counts only, no per-edge
    /// scan, so safe to call cheaply.
    pub fn stats(&self) -> Result<CognitiveStats> {
        Ok(CognitiveStats {
            edges: self.graph.count_edges()?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct CognitiveStats {
    pub edges: usize,
}

// =====================================================================
// Daemon-level singleton + boot helper
// =====================================================================

static INSTANCE: OnceLock<Arc<CognitiveSystem>> = OnceLock::new();
static DECAY_HANDLE: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Construct the `CognitiveSystem` from the current daemon config and DB,
/// install it as the process singleton, and spawn the decay ticker. Safe
/// to call multiple times — subsequent calls are no-ops returning the
/// existing instance.
///
/// Boots the cognitive system. When an embedding provider is configured the
/// full vector + graph + FTS stack comes up. When none is configured but
/// `cognitive.enabled` is on, it boots in **FTS-only mode** (a [`NullEmbedder`]
/// stand-in): keyword/FTS search, file ingestion, recall, and graph browsing
/// all work at zero embedding cost; vector-based search modes return empty
/// until a provider is added. Returns `None` only when the system is
/// explicitly disabled (`cognitive.enabled = false`) and has no provider.
pub fn init_daemon(
    db: Arc<Db>,
    config: &Config,
    llm: Arc<dyn super::llm::LlmClient>,
) -> Option<Arc<CognitiveSystem>> {
    if let Some(existing) = INSTANCE.get() {
        return Some(Arc::clone(existing));
    }
    let provider: Arc<dyn EmbeddingProvider> =
        match create_embedding_provider(config, Arc::clone(&db)) {
            Some(b) => Arc::from(b),
            None => {
                if !config.cognitive.enabled {
                    // Explicitly disabled and nothing to embed with → dormant.
                    return None;
                }
                tracing::info!(
                    "[cognitive] no embedding provider configured — booting in FTS-only mode \
                     (keyword search + ingestion work; vector modes return empty)"
                );
                Arc::new(super::embed::NullEmbedder)
            }
        };
    // Pull the governance knobs from CognitiveConfig so the daemon-built
    // system respects user-tuned `enabled` + `max_concurrent`. Tests still
    // get the defaults via `new()`.
    let graph: Arc<dyn GraphStore> = Arc::new(SqliteGraphStore::new(Arc::clone(&db)));
    let vector: Arc<dyn VectorStore> = Arc::new(SqliteVectorStore::new(db));
    let sys = Arc::new(CognitiveSystem::new_with_limits(
        graph,
        vector,
        provider,
        llm,
        config.cognitive.max_concurrent,
        config.cognitive.enabled,
    ));

    let handle = sys.start_decay(super::decay_tick::DecayConfig::default());
    *DECAY_HANDLE.lock().unwrap() = Some(handle);

    let stored = INSTANCE.get_or_init(|| Arc::clone(&sys));
    Some(Arc::clone(stored))
}

/// Get the live `CognitiveSystem`, or `None` when the daemon never booted it
/// (e.g. embedding provider disabled, or running outside `run_daemon`).
pub fn try_get_instance() -> Option<Arc<CognitiveSystem>> {
    INSTANCE.get().map(Arc::clone)
}

/// Stop the background decay ticker. Idempotent; safe on shutdown.
pub fn shutdown_decay() {
    if let Some(h) = DECAY_HANDLE.lock().unwrap().take() {
        h.abort();
    }
}

/// Format cognitive search hits as a compact context block for LLM prompt
/// injection. Used by AgentPool pre-retrieval. Empty input → empty output
/// so callers can `if !s.is_empty() { ... }` without surrounding logic.
pub fn format_hits_for_prompt(
    hits: &[super::search::SearchHit],
    max_chars_per_hit: usize,
) -> String {
    if hits.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(256);
    for (i, h) in hits.iter().enumerate() {
        let header = if !h.node.name.is_empty() {
            h.node.name.clone()
        } else {
            format!("chunk#{}", i + 1)
        };
        let body = if h.node.summary.is_empty() {
            String::new()
        } else if h.node.summary.len() > max_chars_per_hit {
            format!(
                "\n  {}...",
                crate::util::text::truncate_on_char_boundary(&h.node.summary, max_chars_per_hit)
            )
        } else {
            format!("\n  {}", h.node.summary)
        };
        out.push_str(&format!(
            "- [{:.2}] {} ({}){}\n",
            h.score,
            header,
            h.node.kind.as_str(),
            body
        ));
    }
    out
}

/// Optional per-request override window (e.g. crank up `interval` for tests).
/// Re-exported for convenience so callers don't import the submodule.
pub use super::decay_tick::DecayConfig as DecayCfg;
pub use std::time::Duration as Dur;

#[allow(dead_code)]
fn _silence_unused_duration_warning() {
    let _ = Duration::from_secs(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::memory::cognitive::llm::test_support::StubLlm;
    use async_trait::async_trait;

    struct FakeEmbedder;

    #[async_trait]
    impl EmbeddingProvider for FakeEmbedder {
        fn name(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            "fake-model"
        }
        fn dimensions(&self) -> u32 {
            8
        }
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; 8];
                    for (i, b) in t.bytes().enumerate() {
                        v[i % 8] += b as f32;
                    }
                    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
                    v.iter().map(|x| x / norm).collect()
                })
                .collect())
        }
    }

    fn build_system(replies: Vec<String>) -> CognitiveSystem {
        let cfg = Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbedder);
        let llm: Arc<dyn LlmClient> = Arc::new(StubLlm::new(replies));
        CognitiveSystem::with_sqlite(db, provider, llm)
    }

    #[test]
    fn init_daemon_dormant_when_disabled_without_embedder() {
        // With cognitive DISABLED and no embedding provider, init_daemon stays
        // dormant (returns None, never installs the process-global singleton).
        // The FTS-only boot path (enabled + no provider) is exercised by the
        // retriever tests instead — we avoid it here because it would set the
        // global INSTANCE and race sibling tests.
        let mut cfg = Config::from_env();
        cfg.cognitive.enabled = false;
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());

        struct NoopLlm;
        #[async_trait::async_trait]
        impl super::super::llm::LlmClient for NoopLlm {
            async fn complete(&self, _: &str, _: &str) -> Result<String> {
                anyhow::bail!("noop")
            }
        }
        let llm: Arc<dyn super::super::llm::LlmClient> = Arc::new(NoopLlm);

        if cfg.memory.embedding_provider == crate::config::EmbeddingProvider::None {
            assert!(init_daemon(db, &cfg, llm).is_none());
        }
        // If a real provider was wired (CI env), we don't poke the singleton
        // here — it'd race with other tests that may have set it.
    }

    #[test]
    fn format_hits_for_prompt_empty_returns_empty() {
        let s = format_hits_for_prompt(&[], 100);
        assert!(s.is_empty());
    }

    #[test]
    fn format_hits_for_prompt_includes_score_kind_and_body() {
        use crate::memory::cognitive::data_point::DataPoint;
        use crate::memory::cognitive::search::SearchHit;

        let mut node = DataPoint::entity("Ada", 0);
        node.summary = "computer pioneer".into();
        let hits = vec![SearchHit {
            node,
            score: 0.87,
            path: Vec::new(),
        }];
        let s = format_hits_for_prompt(&hits, 200);
        assert!(s.contains("Ada"), "got: {s}");
        assert!(s.contains("0.87"), "score must appear: {s}");
        assert!(s.contains("entity"), "kind must appear: {s}");
        assert!(s.contains("computer pioneer"), "body must appear: {s}");
    }

    #[test]
    fn format_hits_truncates_long_body() {
        use crate::memory::cognitive::data_point::DataPoint;
        use crate::memory::cognitive::search::SearchHit;

        let mut node = DataPoint::chunk("x".repeat(500), None, 0);
        node.summary = "x".repeat(500);
        let hits = vec![SearchHit {
            node,
            score: 0.5,
            path: Vec::new(),
        }];
        let s = format_hits_for_prompt(&hits, 50);
        assert!(s.contains("..."), "truncation marker expected: {s}");
        // Header + body + trailing ellipsis — keep us well under 500.
        assert!(s.len() < 200, "expected <200 chars, got {}", s.len());
    }

    #[tokio::test]
    async fn end_to_end_via_facade() {
        let canned =
            r#"{"triplets":[{"subject":"Ada","predicate":"invented","object":"compiler"}]}"#
                .to_string();
        let sys = build_system(vec![canned]);
        let _ = sys
            .cognify(
                "Ada invented the compiler.",
                "doc",
                &CognifyOptions::default(),
            )
            .await
            .unwrap();
        let hits = sys
            .search(&SearchQuery::chunks("compiler", 5))
            .await
            .unwrap();
        assert!(!hits.is_empty());
        let stats = sys.stats().unwrap();
        assert!(stats.edges > 0);
        let rep = sys.decay_now().unwrap();
        assert!(rep.edges_scanned > 0);
    }

    #[tokio::test]
    async fn pre_retrieval_recall_then_format_pipeline() {
        // Mirrors what `cognitive_pre_retrieval` in agent_pool/pool.rs does:
        //   1. spreading recall
        //   2. format_hits_for_prompt over the result
        // Verifies the pipeline works without needing the global singleton.
        let canned =
            r#"{"triplets":[{"subject":"Ada","predicate":"invented","object":"compiler"}]}"#
                .to_string();
        let sys = build_system(vec![canned]);
        sys.cognify(
            "Ada invented the compiler.",
            "doc",
            &CognifyOptions::default(),
        )
        .await
        .unwrap();

        let q = SearchQuery::spreading("compiler", 5, 2);
        let hits = sys.search(&q).await.unwrap();
        let filtered: Vec<_> = hits.into_iter().filter(|h| h.score >= 0.0).collect();
        let formatted = format_hits_for_prompt(&filtered, 200);
        assert!(
            !formatted.is_empty(),
            "recall + format must produce a block"
        );
        // The block must be safe to embed inside the <cognitive_memory> tags
        // used by AgentPool — no stray angle-bracket / closing-tag content.
        assert!(!formatted.contains("</cognitive_memory>"));
    }
}

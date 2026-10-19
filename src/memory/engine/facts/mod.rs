//! The fact store and what feeds it.
//!
//! Where the curated notes are written by hand and the graph keeps triplets,
//! this layer keeps *facts*: self-contained statements, dated, with the
//! entities they name. They are extracted from conversation windows by a model
//! (off the request path), scrubbed of secrets first, and read back on the
//! request path with no model at all — four arms (`arms`) that fusion combines
//! with the other stores.
//!
//! It runs beside the triplet extraction, not instead of it. Replacing the
//! triplets is a decision for after the two have been measured against each
//! other on real conversations.

pub mod arms;
pub mod defense;
pub mod extract;
mod restated;
pub mod retain;
pub mod slots;
pub mod store;
pub mod temporal;
pub mod tools;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::OnceCell;

use crate::db::Db;
use crate::memory::embedding::EmbeddingProvider;
use crate::memory::engine::arms::RecallArm;

pub use arms::{GraphArm, KeywordArm, ProjectFacts, SemanticArm, TemporalArm, FACTS};
pub use retain::{retain, RetainReport, RetainRequest};
pub use store::{FactStore, StoredFact};

/// The store, plus the embedding model that gives its facts vectors (none on
/// an install without an embedding provider, which is the default — keyword,
/// graph and temporal recall still work).
pub struct FactEngine {
    pub store: FactStore,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// The last few question embeddings. The semantic arm and the graph arm
    /// run at the same moment and both need the question's vector; this makes
    /// it one call to the embedding model, not two.
    queries: Mutex<VecDeque<(String, Arc<OnceCell<Option<Vec<f32>>>>)>>,
}

/// How many distinct questions' vectors to keep.
const QUERY_CACHE: usize = 4;

impl FactEngine {
    pub fn new(db: Arc<Db>, embedder: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        Self {
            store: FactStore::new(db),
            // 0 dimensions is the "no embeddings" sentinel (cognitive::NullEmbedder).
            embedder: embedder.filter(|e| e.dimensions() > 0),
            queries: Mutex::new(VecDeque::new()),
        }
    }

    /// `provider:model:dimensions`, the identity two vectors must share to be
    /// compared.
    pub fn model_key(&self) -> Option<String> {
        self.embedder
            .as_ref()
            .map(|e| format!("{}:{}:{}", e.name(), e.model(), e.dimensions()))
    }

    /// One vector per text, in order; `None` where there is no embedding model
    /// or it failed. A failure never loses the facts — they are stored without
    /// a vector and still found by keyword, graph and date.
    pub async fn embed_all(&self, texts: &[String]) -> Vec<Option<Vec<f32>>> {
        let none = || vec![None; texts.len()];
        let Some(embedder) = &self.embedder else {
            return none();
        };
        match embedder.embed(texts).await {
            Ok(v) if v.len() == texts.len() => v
                .into_iter()
                .map(|e| (!e.is_empty()).then_some(e))
                .collect(),
            Ok(v) => {
                tracing::warn!(
                    got = v.len(),
                    want = texts.len(),
                    "[facts] embedder returned the wrong count"
                );
                none()
            }
            Err(e) => {
                tracing::warn!(error = %e, "[facts] embedding failed; storing without vectors");
                none()
            }
        }
    }

    /// The vector for a question, computed once however many arms ask for it
    /// at once.
    pub async fn embed_query(&self, text: &str) -> Option<Vec<f32>> {
        let cell = {
            let mut cache = self.queries.lock().unwrap();
            match cache.iter().find(|(t, _)| t == text) {
                Some((_, cell)) => Arc::clone(cell),
                None => {
                    let cell = Arc::new(OnceCell::new());
                    cache.push_back((text.to_string(), Arc::clone(&cell)));
                    if cache.len() > QUERY_CACHE {
                        cache.pop_front();
                    }
                    cell
                }
            }
        };
        cell.get_or_init(|| async { self.embed_all(&[text.to_string()]).await.pop().flatten() })
            .await
            .clone()
    }
}

static INSTANCE: OnceLock<Arc<FactEngine>> = OnceLock::new();

/// Create the process-wide engine (idempotent: the first call wins).
pub fn init(db: Arc<Db>, embedder: Option<Arc<dyn EmbeddingProvider>>) -> Arc<FactEngine> {
    Arc::clone(INSTANCE.get_or_init(|| Arc::new(FactEngine::new(db, embedder))))
}

pub fn instance() -> Option<Arc<FactEngine>> {
    INSTANCE.get().cloned()
}

/// Where a retained text's facts are stored and whom they belong to.
///
/// A chat outside a project writes into its agent's bank tagged with its own
/// JID. A chat in a project writes into the project's bank with no chat at all,
/// so every chat of the project reads, confirms and replaces the others' facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct FactTarget {
    pub bank: String,
    /// The chat the facts are tagged with; empty for the whole bank.
    pub origin: String,
}

impl FactTarget {
    /// The agent's bank, tagged with the chat unless per-chat scoping is off
    /// (`SENCLAW_MEMORY_FACTS_SCOPE=agent`) or the chat is unknown.
    pub fn chat(folder: &str, chat_jid: &str, by_chat: bool) -> Self {
        Self {
            bank: folder.to_string(),
            origin: if by_chat {
                chat_jid.to_string()
            } else {
                String::new()
            },
        }
    }

    /// A bank that belongs to a project as a whole.
    pub fn project(bank: &str) -> Self {
        Self {
            bank: bank.to_string(),
            origin: String::new(),
        }
    }
}

/// The four ways to find a fact in `engine`'s store, then the same four over a
/// project's bank. The second four answer only a question asked from a project
/// (see [`ProjectFacts`]); they cost nothing otherwise.
pub fn fact_arms(engine: Arc<FactEngine>) -> Vec<Arc<dyn RecallArm>> {
    vec![
        Arc::new(SemanticArm(Arc::clone(&engine))),
        Arc::new(KeywordArm(Arc::clone(&engine))),
        Arc::new(GraphArm(Arc::clone(&engine))),
        Arc::new(TemporalArm(Arc::clone(&engine))),
        Arc::new(ProjectFacts::new(
            "facts_project_semantic",
            SemanticArm(Arc::clone(&engine)),
        )),
        Arc::new(ProjectFacts::new(
            "facts_project_keyword",
            KeywordArm(Arc::clone(&engine)),
        )),
        Arc::new(ProjectFacts::new(
            "facts_project_graph",
            GraphArm(Arc::clone(&engine)),
        )),
        Arc::new(ProjectFacts::new(
            "facts_project_temporal",
            TemporalArm(engine),
        )),
    ]
}

/// Retain text into the process-wide engine, with the model the cognitive
/// layer is configured to use. `source` says where it came from
/// (`"reflection"` for a conversation window, `"compaction"` for the summary
/// that replaces history). Does nothing — and says so at debug level — when
/// the engine is not booted or no model is configured.
pub async fn retain_text(
    config: &crate::config::Config,
    target: &FactTarget,
    text: &str,
    source: &str,
) -> Option<RetainReport> {
    let engine = instance()?;
    let Some(llm) = crate::memory::cognitive::create_cognitive_llm(config) else {
        // A window dropped here looks exactly like "nothing worth remembering",
        // which is how a real install went weeks remembering nothing. Say it
        // once at warn; the per-window repeat stays at debug.
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::debug!("[facts] no extraction model usable; window not retained");
        } else {
            tracing::warn!(
                "[facts] no extraction model usable, so conversations are not being remembered: \
                 the model chosen in Settings needs an API key, unless it is a local model or a \
                 server on this machine or network"
            );
        }
        return None;
    };
    let mode = defense::Mode::parse(&config.memory.engine_defense);
    let req = RetainRequest {
        bank: &target.bank,
        text,
        source,
        now: chrono::Utc::now().timestamp(),
        origin: &target.origin,
    };
    Some(retain(&engine, llm.as_ref(), mode, &req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting(Arc<AtomicUsize>, u32);

    #[async_trait]
    impl EmbeddingProvider for Counting {
        fn name(&self) -> &str {
            "counting"
        }
        fn model(&self) -> &str {
            "m"
        }
        fn dimensions(&self) -> u32 {
            self.1
        }
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            // Slow enough that concurrent callers overlap.
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }
    }

    fn engine(calls: &Arc<AtomicUsize>, dims: u32) -> Arc<FactEngine> {
        let db = Arc::new(crate::db::Db::open_in_memory(&Config::from_env()).unwrap());
        Arc::new(FactEngine::new(
            db,
            Some(Arc::new(Counting(Arc::clone(calls), dims))),
        ))
    }

    #[tokio::test]
    async fn two_arms_asking_at_once_cost_one_embedding_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let e = engine(&calls, 2);
        let (a, b) = tokio::join!(e.embed_query("hôm qua"), e.embed_query("hôm qua"));
        assert_eq!(a, Some(vec![1.0, 0.0]));
        assert_eq!(a, b);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The same question again this turn: still cached.
        e.embed_query("hôm qua").await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A different question is a new call.
        e.embed_query("tháng 3").await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_cache_forgets_old_questions() {
        let calls = Arc::new(AtomicUsize::new(0));
        let e = engine(&calls, 2);
        for i in 0..(QUERY_CACHE + 1) {
            e.embed_query(&format!("q{i}")).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), QUERY_CACHE + 1);
        e.embed_query("q0").await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            QUERY_CACHE + 2,
            "q0 was evicted"
        );
    }

    #[tokio::test]
    async fn an_engine_with_no_embedding_model_embeds_nothing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let none = engine(&calls, 0); // 0 dimensions is the "no embeddings" sentinel
        assert_eq!(none.model_key(), None);
        assert_eq!(none.embed_query("anything").await, None);
        assert_eq!(
            none.embed_all(&["a".into(), "b".into()]).await,
            vec![None, None]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn retaining_before_the_engine_boots_does_nothing_and_does_not_fail() {
        // Unit tests never call `init`, so this is the state of a process that
        // has not booted the fact store (a CLI one-shot, an early reflection).
        assert!(instance().is_none());
        let cfg = Config::from_env();
        assert!(retain_text(
            &cfg,
            &FactTarget::chat("main", "web:t", true),
            "User: a conversation window",
            "reflection"
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn the_model_identity_names_provider_model_and_size() {
        let calls = Arc::new(AtomicUsize::new(0));
        assert_eq!(
            engine(&calls, 2).model_key().as_deref(),
            Some("counting:m:2")
        );
    }
}

//! The stores unified recall reads from, one arm each.
//!
//! An arm turns one store's own search into a best-first list of [`Hit`]s and
//! nothing more: no formatting, no budget, no cross-store logic. Those belong
//! to the stages after it, so adding a store means adding an arm.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::memory::cognitive::{CognitiveSystem, NodeSet, SearchHit, SearchQuery};
use crate::memory::curated;
use crate::memory::fts_search::SearchOptions;
use crate::memory::manager::MemoryManager;
use crate::util::text::truncate_on_char_boundary;

use super::types::{Hit, RecallQuery};

pub const CURATED: &str = "curated";
pub const COGNITIVE: &str = "cognitive";

/// A curated note's snippet, in characters.
const NOTE_SNIPPET_CHARS: usize = 300;

/// A graph node's summary, in bytes (cut on a char boundary).
const NODE_SUMMARY_BYTES: usize = 200;

/// Spreading activation below this reads as noise; the context budget is
/// better spent on the user's own prompt.
const MIN_GRAPH_SCORE: f32 = 0.1;

#[async_trait]
pub trait RecallArm: Send + Sync {
    fn name(&self) -> &'static str;

    /// Best first. An error or a timeout costs the turn this arm's hits only.
    async fn search(&self, query: &RecallQuery) -> Result<Vec<Hit>>;
}

// ===== Curated notes =====

/// A curated note that survived the recall filters.
#[derive(Debug, Clone)]
pub(crate) struct NoteCandidate {
    /// `2` this chat's own, `1` shared (see [`curated::memory_session_rank`]).
    pub rank: i32,
    pub path: String,
    pub header: String,
    pub snippet: String,
    pub created_at: Option<i64>,
}

/// Hybrid FTS5/vector search over the folder's curated notes, minus the
/// index, daily logs, and notes written in another chat; same-chat notes
/// first, relevance kept within a tier.
///
/// This is the one place the curated filters live — the legacy prompt block
/// and the unified arm both read through it.
pub(crate) async fn curated_candidates(
    mgr: &MemoryManager,
    query: &str,
    folder: &str,
    chat_jid: &str,
    max_results: usize,
) -> Result<Vec<NoteCandidate>> {
    let opts = SearchOptions {
        max_results: (max_results + 5) * 2,
        min_score: 0.25,
        source: Some("memory".to_owned()),
    };
    let results = mgr.search(folder, query, Some(opts)).await?;

    let today_file = format!("{}.md", chrono::Utc::now().format("%Y-%m-%d"));
    let mut seen = std::collections::HashSet::new();
    let mut ranked: Vec<NoteCandidate> = Vec::new();
    for r in results {
        let file_name = Path::new(&r.path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        if file_name == today_file
            || file_name == "MEMORY.md"
            || curated::is_daily_log_name(&file_name)
            || !seen.insert(r.path.clone())
        {
            continue;
        }
        let meta = curated::read_meta(Path::new(&r.path));
        let rank = match &meta {
            Some(m) => curated::memory_session_rank(&m.origin_session_id, chat_jid, folder),
            None => 1, // no frontmatter → treat as shared
        };
        if rank < 0 {
            continue; // another chat's session-scoped memory
        }
        let created_at = meta.as_ref().and_then(|m| m.created_at);
        let header = match meta {
            Some(m) => format!(
                "{} ({}) — {}",
                m.name,
                if m.mem_type.is_empty() {
                    "memory"
                } else {
                    &m.mem_type
                },
                m.description
            ),
            None => file_name,
        };
        ranked.push(NoteCandidate {
            rank,
            path: r.path,
            header,
            snippet: r.text.chars().take(NOTE_SNIPPET_CHARS).collect(),
            created_at,
        });
    }
    // Stable: same chat first, search relevance preserved inside a tier.
    ranked.sort_by(|a, b| b.rank.cmp(&a.rank));
    Ok(ranked)
}

pub struct CuratedArm {
    manager: Arc<MemoryManager>,
}

impl CuratedArm {
    pub fn new(manager: Arc<MemoryManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl RecallArm for CuratedArm {
    fn name(&self) -> &'static str {
        CURATED
    }

    async fn search(&self, q: &RecallQuery) -> Result<Vec<Hit>> {
        let notes =
            curated_candidates(&self.manager, &q.text, &q.folder, &q.chat_jid, q.limit).await?;
        Ok(notes
            .into_iter()
            .take(q.limit)
            .map(|n| Hit {
                arm: CURATED,
                key: n.path,
                header: n.header,
                body: n.snippet,
                at: n.created_at,
            })
            .collect())
    }
}

// ===== Cognitive graph =====

/// Spreading-activation recall from the folder's default memory scope. Like
/// the prompt block it replaces, it strengthens the edges it walks.
pub(crate) async fn graph_hits(
    sys: &CognitiveSystem,
    query: &str,
    folder: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let mut q = SearchQuery::spreading(query.to_string(), limit, 2);
    q.node_sets = vec![NodeSet::group(folder, "default_memory")];
    let hits = sys.search(&q).await?;
    Ok(hits
        .into_iter()
        .filter(|h| h.score >= MIN_GRAPH_SCORE)
        .collect())
}

/// Vector + keyword seeds with no graph walk and no write-back.
///
/// What the arm falls back to when spreading finds nothing. That is every
/// query on an install with no embedding provider: spreading seeds by vector
/// alone, so without one it comes back empty while the graph's text is still
/// perfectly searchable. Hybrid degrades to keyword search in that case.
async fn keyword_hits(
    sys: &CognitiveSystem,
    query: &str,
    folder: &str,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let mut q = SearchQuery::hybrid(query.to_string(), limit);
    q.node_sets = vec![NodeSet::group(folder, "default_memory")];
    let hits = sys.search(&q).await?;
    Ok(hits
        .into_iter()
        .filter(|h| h.score >= MIN_GRAPH_SCORE)
        .collect())
}

/// A bare entity name says nothing; only hits that carry text are knowledge.
fn carries_text(hit: &SearchHit) -> bool {
    !hit.node.summary.trim().is_empty()
}

pub struct CognitiveArm {
    system: Arc<CognitiveSystem>,
}

impl CognitiveArm {
    pub fn new(system: Arc<CognitiveSystem>) -> Self {
        Self { system }
    }
}

#[async_trait]
impl RecallArm for CognitiveArm {
    fn name(&self) -> &'static str {
        COGNITIVE
    }

    async fn search(&self, q: &RecallQuery) -> Result<Vec<Hit>> {
        let mut hits: Vec<SearchHit> = graph_hits(&self.system, &q.text, &q.folder, q.limit)
            .await?
            .into_iter()
            .filter(carries_text)
            .collect();
        if hits.is_empty() {
            hits = keyword_hits(&self.system, &q.text, &q.folder, q.limit)
                .await?
                .into_iter()
                .filter(carries_text)
                .collect();
        }
        Ok(hits
            .into_iter()
            .map(|h| {
                let header = if h.node.name.is_empty() {
                    format!("{} node", h.node.kind.as_str())
                } else {
                    format!("{} ({})", h.node.name, h.node.kind.as_str())
                };
                Hit {
                    arm: COGNITIVE,
                    key: h.node.id.to_string(),
                    header,
                    body: truncate_on_char_boundary(&h.node.summary, NODE_SUMMARY_BYTES)
                        .to_string(),
                    at: Some(h.node.updated_at),
                }
            })
            .collect())
    }
}

//! Retrievers — port of cognee's `modules/retrieval/*`.
//!
//! Every retriever takes the same `SearchQuery` and returns `Vec<SearchHit>`.
//! All four share a common **seed step**: embed the query text, run vector
//! search, return the top-K nodes. The graph-style modes then walk the graph
//! starting from those seeds.
//!
//! `SpreadingActivation` is the only one that **mutates state**: it calls
//! [`RelationshipEdge::strengthen`] on every edge it traverses, persisting
//! Hebbian write-back. Read-only retrievers use `effective_strength` so they
//! never alter the graph.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use uuid::Uuid;

use super::data_point::{DataPoint, NodeKind};
use super::embed::CognitiveEmbedder;
use super::gnn::GraphScorer;
use super::search::{SearchHit, SearchQuery, SearchType};
use super::triplet::RelationshipEdge;
use crate::memory::query_rewrite::{expand_query_tokens, smart_rewrite_query};
use crate::memory::tokenizer::tokenize_optimized;

/// Build an FTS5 MATCH expression from a natural-language query, reusing the
/// same pipeline as `memory::fts_search`: rewrite → tokenize → expand → strip
/// FTS metacharacters → OR-join. Returns `None` when nothing usable remains
/// (caller treats that as "no FTS hits").
fn build_fts_match(query: &str) -> Option<String> {
    let rewritten = smart_rewrite_query(query);
    let tokens = tokenize_optimized(&rewritten, true);
    if tokens.is_empty() {
        return None;
    }
    let expanded = expand_query_tokens(&tokens);
    let sanitize = |t: &str| -> String {
        t.chars()
            .filter(|c| !matches!(c, '"' | '\'' | '`' | '(' | ')' | '*' | '^' | '-'))
            .collect()
    };
    let joined = expanded
        .iter()
        .map(|t| sanitize(t))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" OR ");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

pub struct CognitiveRetriever {
    pub embedder: Arc<CognitiveEmbedder>,
    scorer: Option<Arc<dyn GraphScorer>>,
}

impl CognitiveRetriever {
    pub fn new(embedder: Arc<CognitiveEmbedder>) -> Self {
        Self {
            embedder,
            scorer: None,
        }
    }

    /// Attach a re-ranker (e.g. `LightGcnScorer`). Activated per call by
    /// setting `SearchQuery::rerank = true`.
    pub fn with_scorer(mut self, scorer: Arc<dyn GraphScorer>) -> Self {
        self.scorer = Some(scorer);
        self
    }

    pub async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let hits = self.search_unfiltered(query).await?;
        // Filtering edges is not enough on its own: the seed step is a vector
        // / FTS lookup over *nodes*, and an outdated value ("149.900") is
        // still a node. Without this pass a superseded price walks straight
        // into the answer as a direct hit, having never crossed an edge.
        self.retain_temporally_visible(hits, query.as_of)
    }

    async fn search_unfiltered(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let hits = if query.node_sets.is_empty() {
            self.dispatch(query).await?
        } else {
            // Scoped search (knowledge space): the seed indexes (vector/FTS)
            // are global, so over-fetch candidates and keep only nodes that
            // are members of any requested set.
            let mut wide = query.clone();
            wide.limit = query.limit.saturating_mul(8).clamp(query.limit, 400);
            let raw = self.dispatch(&wide).await?;
            let allowed = self
                .embedder
                .graph
                .node_ids_in_sets(&query.node_sets)
                .context("resolve node-set membership")?;
            raw.into_iter()
                .filter(|h| allowed.contains(&h.node.id))
                .take(query.limit)
                .collect()
        };
        if query.rerank {
            self.apply_rerank(query, hits).await
        } else {
            Ok(hits)
        }
    }

    /// Drop hits whose every fact was superseded before the moment being
    /// asked about.
    ///
    /// The rule is narrow on purpose: a node is dropped only when it *has*
    /// edges and **none** of them holds at `as_of`. A node with no edges at
    /// all (a freshly extracted entity, a chunk) is not history — it simply
    /// has no facts yet, and hiding it would break ordinary text recall.
    ///
    /// Costs one indexed lookup per surviving hit (a second only for the rare
    /// node with no live edges), over a candidate set already truncated to
    /// the query limit.
    fn retain_temporally_visible(
        &self,
        hits: Vec<SearchHit>,
        as_of: Option<i64>,
    ) -> Result<Vec<SearchHit>> {
        let mut out = Vec::with_capacity(hits.len());
        for hit in hits {
            let live = self.embedder.graph.neighbors_at(hit.node.id, 1, as_of)?;
            if !live.is_empty() {
                out.push(hit);
                continue;
            }
            let any = self.embedder.graph.neighbors(hit.node.id, 1)?;
            if any.is_empty() {
                out.push(hit); // no facts at all — not a historical value
            }
        }
        Ok(out)
    }

    async fn dispatch(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        Ok(match query.query_type {
            SearchType::Chunks => self.search_chunks(query).await?,
            SearchType::Triplet => self.search_triplet(query).await?,
            SearchType::GraphCompletion => self.search_graph_completion(query).await?,
            SearchType::SpreadingActivation => self.search_spreading(query).await?,
            SearchType::Fts => self.search_fts(query)?,
            SearchType::Hybrid => self.search_hybrid(query).await?,
            SearchType::Temporal => self.search_temporal(query).await?,
        })
    }

    /// Re-rank the candidate set using the configured [`GraphScorer`]. If
    /// no scorer is attached, this is a no-op — caller-set `rerank=true`
    /// without a scorer is treated as "off" rather than an error so the
    /// pipeline stays forgiving.
    async fn apply_rerank(
        &self,
        query: &SearchQuery,
        hits: Vec<SearchHit>,
    ) -> Result<Vec<SearchHit>> {
        let Some(scorer) = self.scorer.as_ref() else {
            return Ok(hits);
        };
        if hits.is_empty() {
            return Ok(hits);
        }
        // No embedder (FTS-only) → can't compare query against node vectors.
        if self.embedder.provider.dimensions() == 0 {
            return Ok(hits);
        }

        // Re-embed the query so the scorer compares like-for-like with the
        // stored node embeddings.
        let mut emb = self
            .embedder
            .provider
            .embed(&[query.query_text.clone()])
            .await
            .context("embed query for rerank")?;
        let q = emb.pop().unwrap_or_default();

        // Gather candidate node embeddings from `cog_nodes.embedding` via
        // a single SQL fetch each — caching here would help only when the
        // scorer is hot.
        let candidates: Vec<DataPoint> = hits.iter().map(|h| h.node.clone()).collect();
        let candidate_embs = self.fetch_embeddings(&candidates).await?;
        let new_scores = scorer.score(&q, &candidates, &candidate_embs)?;

        let alpha = query.rerank_alpha.clamp(0.0, 1.0);
        let mut out: Vec<SearchHit> = hits
            .into_iter()
            .zip(new_scores.into_iter())
            .map(|(mut h, s)| {
                h.score = (1.0 - alpha) * h.score + alpha * s;
                h
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(out)
    }

    /// Pull the stored embedding BLOBs for the candidate nodes. Falls back
    /// to a zero vector when a candidate has no embedding (shouldn't happen
    /// after add+embed, but keeps the scorer honest).
    async fn fetch_embeddings(&self, candidates: &[DataPoint]) -> Result<Vec<Vec<f32>>> {
        let dims = self.embedder.provider.dimensions() as usize;
        let mut out = Vec::with_capacity(candidates.len());
        for c in candidates {
            // We don't have a direct "get_embedding(id)" yet — synthesise via
            // a fresh embed of the node's own text. This guarantees correct
            // dim + provider model alignment, at the cost of one extra call
            // per candidate during rerank. The candidate set is intentionally
            // small (<= query.limit) so this is acceptable.
            let text = super::embed::text_for_embedding(c);
            if text.trim().is_empty() {
                out.push(vec![0.0f32; dims]);
                continue;
            }
            let mut v = self.embedder.provider.embed(&[text]).await?;
            out.push(v.pop().unwrap_or_else(|| vec![0.0f32; dims]));
        }
        Ok(out)
    }

    // ---- shared: seed by vector ----

    /// Embed the query and return top-K nearest nodes, optionally filtered
    /// to a kind. `limit` is the requested K — callers usually want a few
    /// more candidates than the final result count.
    async fn vector_seeds(
        &self,
        query_text: &str,
        limit: usize,
        kind_filter: Option<NodeKind>,
    ) -> Result<Vec<(DataPoint, f32)>> {
        // FTS-only mode (NullEmbedder): no vectors exist, so vector recall
        // yields nothing. Hybrid degrades to its FTS half; pure vector modes
        // (Chunks/Triplet/Graph/Spreading) come back empty until a provider
        // is configured.
        if self.embedder.provider.dimensions() == 0 {
            return Ok(Vec::new());
        }
        let mut emb = self
            .embedder
            .provider
            .embed(&[query_text.to_string()])
            .await
            .context("embed query")?;
        let q = emb
            .pop()
            .ok_or_else(|| anyhow::anyhow!("embedder returned empty"))?;
        // Over-fetch when filtering by kind so we still hit `limit` after the filter.
        let fetch = if kind_filter.is_some() {
            limit * 4
        } else {
            limit
        };
        let hits = self.embedder.vector.search(&q, fetch.max(8))?;

        let mut out = Vec::with_capacity(hits.len().min(limit));
        for h in hits {
            if let Some(node) = self.embedder.graph.get_node(h.node_id)? {
                if let Some(k) = kind_filter {
                    if node.kind != k {
                        continue;
                    }
                }
                // distance → similarity score in [0, 1]
                let score = 1.0 - h.distance.clamp(0.0, 2.0) * 0.5;
                out.push((node, score));
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    // ---- shared: seed by full-text (BM25) ----

    /// Top-K nodes by BM25 over `name + summary`. No embedding call — usable
    /// even when the provider is dormant. Returns `(node, score)` with score
    /// in `[0, 1]`. Empty when the query yields no FTS tokens or no matches.
    fn fts_seeds(
        &self,
        query_text: &str,
        limit: usize,
        kind_filter: Option<NodeKind>,
    ) -> Result<Vec<(DataPoint, f32)>> {
        let Some(m) = build_fts_match(query_text) else {
            return Ok(Vec::new());
        };
        self.embedder
            .graph
            .fts_search_nodes(&m, kind_filter.map(|k| k.as_str()), limit)
            .context("fts_search_nodes")
    }

    // ---- Chunks ----

    async fn search_chunks(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let seeds = self
            .vector_seeds(&query.query_text, query.limit, Some(NodeKind::Chunk))
            .await?;
        Ok(seeds
            .into_iter()
            .map(|(node, score)| SearchHit {
                node,
                score,
                path: Vec::new(),
            })
            .collect())
    }

    // ---- Triplet ----

    async fn search_triplet(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let seeds = self
            .vector_seeds(&query.query_text, query.limit, Some(NodeKind::Entity))
            .await?;
        let now = Utc::now().timestamp();
        let mut hits = Vec::new();
        for (entity, seed_score) in seeds {
            let edges = self
                .embedder
                .graph
                .neighbors_at(entity.id, 32, query.as_of)
                .context("neighbors for triplet")?;
            // The seed entity itself first.
            hits.push(SearchHit {
                node: entity.clone(),
                score: seed_score,
                path: Vec::new(),
            });
            for edge in edges {
                if edge.predicate == "MENTIONS" {
                    continue; // skip provenance edges in TRIPLET view
                }
                let neighbor_id = if edge.src == entity.id {
                    edge.dst
                } else {
                    edge.src
                };
                if let Some(nbr) = self.embedder.graph.get_node(neighbor_id)? {
                    let strength = edge.effective_strength(now);
                    hits.push(SearchHit {
                        node: nbr,
                        score: seed_score * strength,
                        path: vec![edge],
                    });
                }
            }
            if hits.len() >= query.limit * 4 {
                break;
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(query.limit);
        Ok(hits)
    }

    // ---- Temporal (facts as of a moment in world time) ----

    /// Like `Triplet`, but ranked by *when* rather than only by strength.
    ///
    /// Seeds come from vector + FTS (a question about a past state often
    /// names the entity literally — "giá vàng BTMC ngày 31/07" — and the
    /// keyword half is what finds it when the embedder is dormant). Edges are
    /// filtered to the requested moment by `neighbors_at`, then scored with a
    /// proximity term so the fact that was in force closest to `as_of` wins
    /// over one that merely has a high Hebbian strength.
    async fn search_temporal(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let over = (query.limit * 2).max(query.limit);
        let mut seeds = self
            .vector_seeds(&query.query_text, over, Some(NodeKind::Entity))
            .await
            .unwrap_or_default();
        if seeds.is_empty() {
            seeds = self.fts_seeds(&query.query_text, over, None)?;
        }

        let now = Utc::now().timestamp();
        let at = query.as_of.unwrap_or(now);
        let mut hits: Vec<SearchHit> = Vec::new();
        let mut seen: HashSet<Uuid> = HashSet::new();

        for (entity, seed_score) in seeds {
            let edges = self
                .embedder
                .graph
                .neighbors_at(entity.id, 32, query.as_of)
                .context("neighbors for temporal")?;
            if seen.insert(entity.id) {
                hits.push(SearchHit {
                    node: entity.clone(),
                    score: seed_score,
                    path: Vec::new(),
                });
            }
            for edge in edges {
                if edge.predicate == "MENTIONS" {
                    continue; // provenance, not a fact
                }
                let neighbor_id = if edge.src == entity.id {
                    edge.dst
                } else {
                    edge.src
                };
                let Some(nbr) = self.embedder.graph.get_node(neighbor_id)? else {
                    continue;
                };
                let score = seed_score * temporal_proximity(&edge, at);
                hits.push(SearchHit {
                    node: nbr,
                    score,
                    path: vec![edge],
                });
            }
        }

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(query.limit);
        Ok(hits)
    }

    // ---- GraphCompletion (read-only k-hop) ----

    async fn search_graph_completion(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        // Entities first — they form the backbone of the subgraph. If we
        // don't find enough entity seeds, fall back to chunk seeds and walk
        // their MENTIONS edges.
        let mut seeds = self
            .vector_seeds(&query.query_text, query.limit, Some(NodeKind::Entity))
            .await?;
        if seeds.is_empty() {
            seeds = self
                .vector_seeds(&query.query_text, query.limit, None)
                .await?;
        }
        self.walk(seeds, query, false).await
    }

    // ---- SpreadingActivation (Hebbian write-back) ----

    async fn search_spreading(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let seeds = self
            .vector_seeds(&query.query_text, query.limit, None)
            .await?;
        self.walk(seeds, query, true).await
    }

    // ---- Fts (BM25, no embeddings) ----

    fn search_fts(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        // No kind filter: both entities and chunks are useful surface hits.
        let seeds = self.fts_seeds(&query.query_text, query.limit, None)?;
        Ok(seeds
            .into_iter()
            .map(|(node, score)| SearchHit {
                node,
                score,
                path: Vec::new(),
            })
            .collect())
    }

    // ---- Hybrid (vector 0.7 + FTS 0.3, deduped) ----

    async fn search_hybrid(&self, query: &SearchQuery) -> Result<Vec<SearchHit>> {
        let over = (query.limit * 2).max(query.limit);
        // Vector half is best-effort: if the provider is dormant or errors,
        // we degrade to FTS-only rather than failing the whole search.
        let vec_seeds = self
            .vector_seeds(&query.query_text, over, None)
            .await
            .unwrap_or_default();
        let fts_seeds = self.fts_seeds(&query.query_text, over, None)?;

        // Merge by node id, weighting vector 0.7 / FTS 0.3 (mirrors
        // `memory::fts_search::mixed_search`). The two score scales aren't
        // calibrated, so this is best-effort blending, not a true RRF.
        let mut combined: HashMap<Uuid, SearchHit> = HashMap::new();
        for (node, score) in vec_seeds {
            combined.insert(
                node.id,
                SearchHit {
                    node,
                    score: score * 0.7,
                    path: Vec::new(),
                },
            );
        }
        for (node, score) in fts_seeds {
            combined
                .entry(node.id)
                .and_modify(|h| h.score += score * 0.3)
                .or_insert(SearchHit {
                    node,
                    score: score * 0.3,
                    path: Vec::new(),
                });
        }

        let mut out: Vec<SearchHit> = combined.into_values().collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(query.limit);
        Ok(out)
    }

    /// Shared k-hop BFS used by GraphCompletion (read_only=true) and
    /// SpreadingActivation (read_only=false → strengthen as we go).
    async fn walk(
        &self,
        seeds: Vec<(DataPoint, f32)>,
        query: &SearchQuery,
        write_back: bool,
    ) -> Result<Vec<SearchHit>> {
        let now = Utc::now().timestamp();
        // node_id → (best activation, best path)
        let mut best: HashMap<Uuid, (f32, Vec<RelationshipEdge>)> = HashMap::new();
        let mut frontier: Vec<(Uuid, f32, Vec<RelationshipEdge>)> = Vec::new();

        for (node, score) in &seeds {
            best.insert(node.id, (*score, Vec::new()));
            frontier.push((node.id, *score, Vec::new()));
        }

        let decay = query.decay_per_hop.clamp(0.05, 1.0);
        for _hop in 0..query.hops {
            let mut next: Vec<(Uuid, f32, Vec<RelationshipEdge>)> = Vec::new();
            for (node_id, activation, path) in frontier.drain(..) {
                // Temporal gate for the whole traversal: `as_of = None`
                // (the default) hides facts a later one superseded, so a
                // spreading recall cannot surface last week's price.
                let edges = self.embedder.graph.neighbors_at(node_id, 64, query.as_of)?;
                for mut edge in edges {
                    let neighbor_id = if edge.src == node_id {
                        edge.dst
                    } else {
                        edge.src
                    };
                    let strength = edge.effective_strength(now);
                    let propagated = activation * decay * strength;
                    if propagated < 0.01 {
                        continue;
                    }

                    if write_back {
                        // Hebbian: passing activation through this edge
                        // strengthens it. Importance scaled by activation so
                        // strong signals reinforce more than weak ones.
                        edge.strengthen(activation.clamp(0.1, 1.0), now);
                        self.embedder.graph.upsert_edge(&edge)?;
                    }

                    let mut new_path = path.clone();
                    new_path.push(edge);

                    let entry = best
                        .entry(neighbor_id)
                        .or_insert((f32::NEG_INFINITY, Vec::new()));
                    if propagated > entry.0 {
                        *entry = (propagated, new_path.clone());
                        next.push((neighbor_id, propagated, new_path));
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }

        let mut hits: Vec<SearchHit> = Vec::with_capacity(best.len());
        for (id, (score, path)) in best.drain() {
            if let Some(node) = self.embedder.graph.get_node(id)? {
                hits.push(SearchHit { node, score, path });
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(query.limit);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::memory::cognitive::cognify::{CognifyOptions, CognifyPipeline};
    use crate::memory::cognitive::embed::CognitiveEmbedder;
    use crate::memory::cognitive::graph_store::SqliteGraphStore;
    use crate::memory::cognitive::llm::test_support::StubLlm;
    use crate::memory::cognitive::vector_store::SqliteVectorStore;
    use crate::memory::embedding::EmbeddingProvider;
    use async_trait::async_trait;

    // Re-import the trait that was removed from the main module
    use crate::memory::cognitive::graph_store::GraphStore;
    use crate::memory::cognitive::vector_store::VectorStore;

    /// Deterministic embedder: bag-of-bytes hash → 8-dim vector. Distinct
    /// inputs land at distinct points, identical inputs collide exactly.
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
        async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
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

    /// Build a small fixture: cognify a sentence that yields two triplets
    /// (Ada -[invented]→ compiler, compiler -[runs_on]→ machine).
    async fn fixture() -> (Arc<CognitiveEmbedder>, CognifyPipeline) {
        let cfg = Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let graph: Arc<dyn GraphStore> = Arc::new(SqliteGraphStore::new(Arc::clone(&db)));
        let vector: Arc<dyn VectorStore> = Arc::new(SqliteVectorStore::new(Arc::clone(&db)));
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbedder);
        let embedder = Arc::new(CognitiveEmbedder::new(graph, vector, provider));
        let canned = r#"{"triplets":[
            {"subject":"Ada","predicate":"invented","object":"compiler"},
            {"subject":"compiler","predicate":"runs_on","object":"machine"}
        ]}"#
        .to_string();
        let llm = Arc::new(StubLlm::new(vec![canned]));
        let pipe = CognifyPipeline::new(
            CognitiveEmbedder::new(
                Arc::clone(&embedder.graph),
                Arc::clone(&embedder.vector),
                Arc::clone(&embedder.provider),
            ),
            llm,
        );
        pipe.cognify(
            "Ada invented the compiler. The compiler runs on the machine.",
            "doc",
            &CognifyOptions::default(),
        )
        .await
        .unwrap();
        (embedder, pipe)
    }

    /// Two prices for one shop, the first superseded a day ago — the shape
    /// the graph was silently accumulating before supersession existed.
    async fn price_fixture() -> Arc<CognitiveEmbedder> {
        let cfg = Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let graph: Arc<dyn GraphStore> = Arc::new(SqliteGraphStore::new(Arc::clone(&db)));
        let vector: Arc<dyn VectorStore> = Arc::new(SqliteVectorStore::new(Arc::clone(&db)));
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbedder);
        let embedder = Arc::new(CognitiveEmbedder::new(graph, vector, provider));

        let now = Utc::now().timestamp();
        let day = 86_400;
        let shop = DataPoint::entity("BTMC", now - 3 * day);
        let old_price = DataPoint::entity("149.900", now - 3 * day);
        let new_price = DataPoint::entity("141.500", now);
        for n in [&shop, &old_price, &new_price] {
            embedder.add_node(n).await.unwrap();
        }

        let mut old = RelationshipEdge::new(shop.id, old_price.id, "sell_price", now - 3 * day);
        old.last_activated = now - 3 * day;
        old.strength = 0.9; // deliberately the *stronger* edge
        old.invalidate(now - day, Uuid::new_v4());
        embedder.graph.upsert_edge(&old).unwrap();

        let mut new = RelationshipEdge::new(shop.id, new_price.id, "sell_price", now - day);
        new.last_activated = now - day;
        new.strength = 0.3;
        embedder.graph.upsert_edge(&new).unwrap();
        embedder
    }

    // The guarantee the whole feature exists for: recall answers with the
    // fact that is true now, even when the outdated one is the stronger edge.
    #[tokio::test]
    async fn recall_never_surfaces_a_superseded_fact() {
        let embedder = price_fixture().await;
        let r = CognitiveRetriever::new(embedder);

        for q in [
            SearchQuery::spreading("BTMC", 10, 2),
            SearchQuery::graph_completion("BTMC", 10, 2),
            SearchQuery::triplet("BTMC", 10),
            SearchQuery::temporal("BTMC", 10, None),
        ] {
            let mode = format!("{:?}", q.query_type);
            let names: Vec<String> = r
                .search(&q)
                .await
                .unwrap()
                .into_iter()
                .map(|h| h.node.name)
                .collect();
            assert!(
                names.iter().any(|n| n == "141.500"),
                "{mode}: current price missing; got {names:?}"
            );
            assert!(
                !names.iter().any(|n| n == "149.900"),
                "{mode}: superseded price leaked into a present-tense recall; got {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn as_of_reaches_the_fact_that_was_true_then() {
        let embedder = price_fixture().await;
        let r = CognitiveRetriever::new(embedder);
        let two_days_ago = Utc::now().timestamp() - 2 * 86_400;

        let names: Vec<String> = r
            .search(&SearchQuery::temporal("BTMC", 10, Some(two_days_ago)))
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.node.name)
            .collect();
        assert!(
            names.iter().any(|n| n == "149.900"),
            "time travel must return the price in force then; got {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "141.500"),
            "a fact asserted later must not appear in the past; got {names:?}"
        );
    }

    // Time travel is opt-in per call and must not leak into the next query.
    #[tokio::test]
    async fn as_of_does_not_change_the_default_answer() {
        let embedder = price_fixture().await;
        let r = CognitiveRetriever::new(embedder);
        let past = SearchQuery::temporal("BTMC", 10, Some(Utc::now().timestamp() - 2 * 86_400));
        let _ = r.search(&past).await.unwrap();

        let names: Vec<String> = r
            .search(&SearchQuery::temporal("BTMC", 10, None))
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.node.name)
            .collect();
        assert!(names.iter().any(|n| n == "141.500"));
        assert!(!names.iter().any(|n| n == "149.900"));
    }

    #[tokio::test]
    async fn chunks_retriever_returns_chunk_nodes() {
        let (embedder, _) = fixture().await;
        let r = CognitiveRetriever::new(embedder);
        let hits = r.search(&SearchQuery::chunks("compiler", 5)).await.unwrap();
        assert!(!hits.is_empty(), "expected at least one chunk hit");
        assert!(hits.iter().all(|h| h.node.kind == NodeKind::Chunk));
    }

    #[tokio::test]
    async fn triplet_retriever_returns_entities_and_edges() {
        let (embedder, _) = fixture().await;
        let r = CognitiveRetriever::new(embedder);
        let hits = r
            .search(&SearchQuery::triplet("compiler", 10))
            .await
            .unwrap();
        assert!(!hits.is_empty());
        // At least one hit should carry an outgoing edge.
        assert!(hits.iter().any(|h| !h.path.is_empty()));
    }

    #[tokio::test]
    async fn graph_completion_walks_multiple_hops() {
        let (embedder, _) = fixture().await;
        let r = CognitiveRetriever::new(embedder);
        // 2 hops should reach `machine` from a `Ada` seed (Ada→compiler→machine).
        let hits = r
            .search(&SearchQuery::graph_completion("Ada", 10, 2))
            .await
            .unwrap();
        let names: Vec<String> = hits
            .iter()
            .filter(|h| h.node.kind == NodeKind::Entity)
            .map(|h| h.node.name.clone())
            .collect();
        assert!(
            names.iter().any(|n| n == "machine"),
            "expected to reach 'machine'; got {names:?}"
        );
    }

    #[tokio::test]
    async fn fts_retriever_returns_hits() {
        let (embedder, _) = fixture().await;
        let r = CognitiveRetriever::new(embedder);
        let hits = r.search(&SearchQuery::fts("compiler", 5)).await.unwrap();
        assert!(!hits.is_empty(), "expected FTS hits for 'compiler'");
        assert!(hits.iter().all(|h| h.path.is_empty()));
        assert!(hits.iter().any(|h| {
            h.node.name.to_lowercase().contains("compiler")
                || h.node.summary.to_lowercase().contains("compiler")
        }));
    }

    #[tokio::test]
    async fn hybrid_retriever_merges_vector_and_fts() {
        let (embedder, _) = fixture().await;
        let r = CognitiveRetriever::new(embedder);
        let hits = r.search(&SearchQuery::hybrid("compiler", 5)).await.unwrap();
        assert!(!hits.is_empty(), "expected hybrid hits");
        // Scores stay sorted descending after the merge.
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score - 1e-6));
    }

    #[tokio::test]
    async fn fts_only_mode_works_without_embeddings() {
        use crate::memory::cognitive::embed::NullEmbedder;

        // Build the whole pipeline around a NullEmbedder (dimensions() == 0):
        // the FTS-only boot path. No vectors are ever written.
        let cfg = Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let graph: Arc<dyn GraphStore> = Arc::new(SqliteGraphStore::new(Arc::clone(&db)));
        let vector: Arc<dyn VectorStore> = Arc::new(SqliteVectorStore::new(Arc::clone(&db)));
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(NullEmbedder);
        let embedder = Arc::new(CognitiveEmbedder::new(graph, vector, provider));
        let canned =
            r#"{"triplets":[{"subject":"Ada","predicate":"invented","object":"compiler"}]}"#
                .to_string();
        let llm = Arc::new(StubLlm::new(vec![canned]));
        let pipe = CognifyPipeline::new(
            CognitiveEmbedder::new(
                Arc::clone(&embedder.graph),
                Arc::clone(&embedder.vector),
                Arc::clone(&embedder.provider),
            ),
            llm,
        );
        // Ingest succeeds with no embedder — nodes are stored + FTS-indexed.
        pipe.cognify(
            "Ada invented the compiler.",
            "doc",
            &CognifyOptions::default(),
        )
        .await
        .unwrap();

        let r = CognitiveRetriever::new(embedder);

        // FTS recall works at zero embedding cost.
        let fts = r.search(&SearchQuery::fts("compiler", 5)).await.unwrap();
        assert!(
            !fts.is_empty(),
            "FTS must find 'compiler' without an embedder"
        );

        // Hybrid degrades to its FTS half (vector half is empty).
        let hybrid = r.search(&SearchQuery::hybrid("compiler", 5)).await.unwrap();
        assert!(!hybrid.is_empty(), "hybrid should fall back to FTS results");

        // Pure vector modes return empty — no vectors were written.
        let chunks = r.search(&SearchQuery::chunks("compiler", 5)).await.unwrap();
        assert!(
            chunks.is_empty(),
            "vector search must be empty in FTS-only mode"
        );
    }

    #[tokio::test]
    async fn rerank_runs_without_scorer_attached_is_noop() {
        // rerank=true + no scorer → results returned untouched (forgiving).
        let (embedder, _) = fixture().await;
        let r = CognitiveRetriever::new(embedder);
        let mut q = SearchQuery::chunks("compiler", 5);
        q.rerank = true;
        let hits = r.search(&q).await.unwrap();
        assert!(!hits.is_empty());
    }

    #[tokio::test]
    async fn rerank_with_lightgcn_changes_scores() {
        use super::super::gnn::LightGcnScorer;

        let (embedder, _) = fixture().await;
        let scorer: Arc<dyn super::super::gnn::GraphScorer> =
            Arc::new(LightGcnScorer::new(Arc::clone(&embedder.graph)).with_layers(2));
        let r = CognitiveRetriever::new(Arc::clone(&embedder)).with_scorer(scorer);

        let mut q = SearchQuery::graph_completion("Ada", 10, 2);
        let baseline = r.search(&q).await.unwrap();
        q.rerank = true;
        q.rerank_alpha = 0.7;
        let reranked = r.search(&q).await.unwrap();

        // The two result sets should overlap heavily but at least one score
        // should differ — proof the LightGCN blend actually ran.
        let baseline_scores: Vec<(uuid::Uuid, f32)> =
            baseline.iter().map(|h| (h.node.id, h.score)).collect();
        let reranked_scores: Vec<(uuid::Uuid, f32)> =
            reranked.iter().map(|h| (h.node.id, h.score)).collect();
        assert!(!baseline_scores.is_empty());
        assert!(!reranked_scores.is_empty());
        let any_diff = reranked_scores.iter().any(|(id, s_new)| {
            baseline_scores
                .iter()
                .find(|(bid, _)| bid == id)
                .map(|(_, s_old)| (s_new - s_old).abs() > 1e-4)
                .unwrap_or(true)
        });
        assert!(
            any_diff,
            "expected at least one score to change after rerank"
        );
    }

    #[tokio::test]
    async fn spreading_activation_writes_back() {
        let (embedder, _) = fixture().await;

        // Snapshot a known edge's activation count BEFORE spreading.
        let ada = embedder.graph.find_entity_by_name("Ada").unwrap().unwrap();
        let before = embedder
            .graph
            .neighbors(ada.id, 16)
            .unwrap()
            .into_iter()
            .find(|e| e.predicate == "invented")
            .expect("invented edge");
        let before_count = before.activation_count;

        let r = CognitiveRetriever::new(Arc::clone(&embedder));
        let _ = r
            .search(&SearchQuery::spreading("Ada", 10, 2))
            .await
            .unwrap();

        let after = embedder
            .graph
            .neighbors(ada.id, 16)
            .unwrap()
            .into_iter()
            .find(|e| e.predicate == "invented")
            .expect("invented edge");
        assert!(
            after.activation_count > before_count,
            "spreading activation should strengthen edges (before={before_count}, after={})",
            after.activation_count
        );
    }
}

/// How much a fact counts when the question is "as of `at`".
///
/// The Hebbian `strength` answers "how live is this knowledge", which is the
/// wrong axis for a dated question — a price mentioned a hundred times still
/// isn't the price on the day being asked about. So proximity dominates and
/// strength only breaks ties:
///
/// * in force at `at` (`valid_from <= at < valid_to`) → full marks, minus a
///   gentle penalty for having started long before, so the *most recent* fact
///   still standing at that moment wins over an older one that also held;
/// * not yet asserted, or already superseded → heavily discounted rather than
///   dropped: `neighbors_at` has already filtered, and anything reaching here
///   is a boundary case worth showing below the real answers.
fn temporal_proximity(edge: &RelationshipEdge, at: i64) -> f32 {
    const WEEK: f32 = 7.0 * 86_400.0;
    let strength_tiebreak = 0.15 * edge.effective_strength(at).clamp(0.0, 1.0);
    if !edge.is_valid_at(at) {
        return 0.05 + strength_tiebreak;
    }
    // Age of the assertion at the asked-about moment, in weeks, squashed to
    // (0, 1]. A fact asserted the same day scores ~1.0; one from a month
    // earlier that is still in force scores ~0.2 — present, but outranked.
    let age_weeks = ((at - edge.valid_from).max(0) as f32) / WEEK;
    let recency = 1.0 / (1.0 + age_weeks);
    (0.85 * recency + strength_tiebreak).clamp(0.0, 1.0)
}

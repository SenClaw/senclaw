//! Graph Neural Network re-ranker — port of LightGCN's **no-training**
//! variant for cognitive memory retrieval.
//!
//! ## Why LightGCN (not GraphSAGE)?
//!
//! GraphSAGE needs trained per-layer weights. We don't have a labelled
//! cognitive-memory dataset, and training one would need a recall benchmark
//! the user doesn't have. LightGCN's contribution is the observation that
//! *the trained per-layer weights add almost nothing* — the lift comes from
//! iterated symmetric-normalized neighborhood averaging.
//!
//! Algorithm:
//!
//! ```text
//!   h^(0)_i  =  embedding(node_i)
//!   h^(k+1)_i = Σ_j∈N(i)  w_ij / sqrt(deg(i) * deg(j))  ·  h^(k)_j
//!   h_final = mean(h^(0), …, h^(K))
//!   score   = cosine(query, h_final)
//! ```
//!
//! Edge weight `w_ij` = `RelationshipEdge::effective_strength` so Hebbian
//! dynamics flow into the re-ranker without extra plumbing. K = 2 by
//! default (matches LightGCN paper's sweet spot).
//!
//! ## Where it plugs in
//!
//! Re-ranks the top-K candidates after a retriever's vector / graph walk
//! has already narrowed the field — typically 20–50 nodes. At that size,
//! pure-Rust f32 is fast enough that MLX acceleration would be overkill.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use uuid::Uuid;

use super::data_point::DataPoint;
use super::graph_store::GraphStore;

/// Re-rank a candidate list using graph structure.
///
/// Implementations receive the query embedding plus the candidate set; they
/// return per-candidate scores in the same order. Higher = better.
pub trait GraphScorer: Send + Sync {
    fn score(
        &self,
        query_emb: &[f32],
        candidates: &[DataPoint],
        candidate_embs: &[Vec<f32>],
    ) -> Result<Vec<f32>>;
}

/// LightGCN-style propagation. No training; uses existing node embeddings
/// + edge strengths to do K-hop weighted neighborhood smoothing.
pub struct LightGcnScorer {
    graph: Arc<dyn GraphStore>,
    /// Number of propagation layers. 2 is the LightGCN default sweet spot.
    pub layers: usize,
    /// Cap on neighbors per node per layer — defensive against high-degree
    /// nodes (hub entities) dominating the propagated representation.
    pub max_neighbors: usize,
    /// Skip self-loops in propagation. LightGCN omits them by default.
    pub skip_self: bool,
}

impl LightGcnScorer {
    pub fn new(graph: Arc<dyn GraphStore>) -> Self {
        Self {
            graph,
            layers: 2,
            max_neighbors: 32,
            skip_self: true,
        }
    }

    pub fn with_layers(mut self, k: usize) -> Self {
        self.layers = k.max(1);
        self
    }
}

// =====================================================================
// Math helpers — pure, no graph IO, directly testable.
// =====================================================================

pub(crate) fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let d = (na.sqrt() * nb.sqrt()).max(1e-12);
    dot / d
}

pub(crate) fn add_scaled(dst: &mut [f32], src: &[f32], scale: f32) {
    debug_assert_eq!(dst.len(), src.len());
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d += s * scale;
    }
}

/// In-place average: `dst[i] /= n` when n > 0.
pub(crate) fn scale_inplace(v: &mut [f32], inv: f32) {
    for x in v.iter_mut() {
        *x *= inv;
    }
}

/// Build symmetric-normalized neighborhood weights for one layer.
/// Returns `(neighbor_emb_index, weight)` pairs per candidate.
///
/// `degrees[i]` is the (precomputed) effective degree of node i, used to
/// keep the normalization stable across layers.
pub(crate) fn symmetric_norm_weights(
    edges: &[(usize, usize, f32)], // (from_idx, to_idx, edge_weight)
    degrees: &[f32],
) -> Vec<Vec<(usize, f32)>> {
    let n = degrees.len();
    let mut out: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    for &(i, j, w) in edges {
        if i >= n || j >= n {
            continue;
        }
        let di = degrees[i].max(1e-6);
        let dj = degrees[j].max(1e-6);
        let norm = w / (di * dj).sqrt();
        out[i].push((j, norm));
    }
    out
}

// =====================================================================
// LightGCN propagation
// =====================================================================

impl GraphScorer for LightGcnScorer {
    fn score(
        &self,
        query_emb: &[f32],
        candidates: &[DataPoint],
        candidate_embs: &[Vec<f32>],
    ) -> Result<Vec<f32>> {
        if candidates.is_empty() || candidate_embs.is_empty() {
            return Ok(Vec::new());
        }
        let dim = candidate_embs[0].len();
        if query_emb.len() != dim {
            anyhow::bail!(
                "LightGcn: query dim {} != candidate dim {}",
                query_emb.len(),
                dim
            );
        }

        let n = candidates.len();
        let id_to_idx: HashMap<Uuid, usize> = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id, i))
            .collect();

        // Collect the induced subgraph between candidates. We deliberately
        // stay within the candidate set: re-ranking should respect the
        // retriever's recall decision instead of widening it.
        let now = chrono::Utc::now().timestamp();
        let mut edges_raw: Vec<(usize, usize, f32)> = Vec::new();
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        for (i, c) in candidates.iter().enumerate() {
            let nbrs = self.graph.neighbors(c.id, self.max_neighbors)?;
            for e in nbrs {
                let other = if e.src == c.id { e.dst } else { e.src };
                if self.skip_self && other == c.id {
                    continue;
                }
                if let Some(&j) = id_to_idx.get(&other) {
                    if seen.insert((i, j)) {
                        let w = e.effective_strength(now).clamp(1e-3, 1.0);
                        edges_raw.push((i, j, w));
                    }
                }
            }
        }
        // Force symmetry so the normalization makes sense even for asymmetric
        // predicates: every (i,j) gets a mirror (j,i) if not already present.
        let mirror: Vec<(usize, usize, f32)> = edges_raw
            .iter()
            .filter_map(|&(i, j, w)| {
                if !seen.contains(&(j, i)) {
                    Some((j, i, w))
                } else {
                    None
                }
            })
            .collect();
        for e in &mirror {
            seen.insert((e.0, e.1));
        }
        edges_raw.extend(mirror);

        // Degree = sum of incident weights.
        let mut degrees = vec![0.0f32; n];
        for &(i, _, w) in &edges_raw {
            degrees[i] += w;
        }
        let norm_neigh = symmetric_norm_weights(&edges_raw, &degrees);

        // h0 = candidate embeddings (defensive copy so we can layer-mean below).
        let mut layers_acc: Vec<Vec<f32>> = candidate_embs
            .iter()
            .map(|v| {
                let mut o = vec![0.0f32; dim];
                // Optional safety: pad/truncate to dim. We already checked
                // length above; this just preserves shape in case of bad input.
                let take = v.len().min(dim);
                o[..take].copy_from_slice(&v[..take]);
                o
            })
            .collect();

        let mut cur: Vec<Vec<f32>> = layers_acc.clone();

        for _layer in 0..self.layers {
            let mut next: Vec<Vec<f32>> = vec![vec![0.0f32; dim]; n];
            for i in 0..n {
                for &(j, w) in &norm_neigh[i] {
                    add_scaled(&mut next[i], &cur[j], w);
                }
            }
            // accumulate then advance
            for i in 0..n {
                for d in 0..dim {
                    layers_acc[i][d] += next[i][d];
                }
            }
            cur = next;
        }

        // Average across (K+1) layer states.
        let inv = 1.0 / (self.layers as f32 + 1.0);
        for v in layers_acc.iter_mut() {
            scale_inplace(v, inv);
        }

        Ok(layers_acc
            .iter()
            .map(|h| cosine_sim(query_emb, h))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::memory::cognitive::graph_store::SqliteGraphStore;
    use crate::memory::cognitive::triplet::RelationshipEdge;
    use std::sync::Arc;

    fn fresh_graph() -> Arc<SqliteGraphStore> {
        let cfg = Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        Arc::new(SqliteGraphStore::new(db))
    }

    #[test]
    fn cosine_sim_identical_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        assert!((cosine_sim(&a, &a) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_sim_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_sim(&a, &b).abs() < 1e-5);
    }

    #[test]
    fn symmetric_norm_weights_basic() {
        // 2-node, 1 edge: degrees = [w, w]; weight = w / sqrt(w*w) = 1
        let edges = vec![(0, 1, 0.5)];
        let degrees = vec![0.5, 0.5];
        let nw = symmetric_norm_weights(&edges, &degrees);
        assert_eq!(nw[0].len(), 1);
        assert!((nw[0][0].1 - 1.0).abs() < 1e-5);
    }

    #[test]
    fn lightgcn_propagates_along_path() {
        // 3-node line: a -[r]- b -[r]- c
        // Initial: only `a` is similar to query.
        // After LightGCN, `b` should beat `c` because b is 1 hop from a, c is 2 hops.
        let g = fresh_graph();
        let now = chrono::Utc::now().timestamp();

        use crate::memory::cognitive::data_point::DataPoint;
        let a = DataPoint::entity("A", now);
        let b = DataPoint::entity("B", now);
        let c = DataPoint::entity("C", now);
        g.upsert_node(&a).unwrap();
        g.upsert_node(&b).unwrap();
        g.upsert_node(&c).unwrap();

        let mk_edge = |from: Uuid, to: Uuid| {
            let mut e = RelationshipEdge::new(from, to, "rel", now);
            e.strength = 0.9;
            e.last_activated = now;
            e
        };
        g.upsert_edge(&mk_edge(a.id, b.id)).unwrap();
        g.upsert_edge(&mk_edge(b.id, c.id)).unwrap();

        let scorer = LightGcnScorer::new(Arc::clone(&g) as Arc<dyn GraphStore>);
        let candidates = vec![a.clone(), b.clone(), c.clone()];

        let query = vec![1.0, 0.0, 0.0];
        let embs = vec![
            vec![1.0, 0.0, 0.0], // A — exact match
            vec![0.0, 1.0, 0.0], // B — orthogonal initially
            vec![0.0, 0.0, 1.0], // C — orthogonal initially
        ];

        let raw = embs
            .iter()
            .map(|v| cosine_sim(&query, v))
            .collect::<Vec<_>>();
        assert!(raw[0] > raw[1] && raw[0] > raw[2]);
        assert!(
            (raw[1] - raw[2]).abs() < 1e-5,
            "B and C tie before propagation"
        );

        let scored = scorer.score(&query, &candidates, &embs).unwrap();
        // After propagation, B (1-hop from A) should outscore C (2-hop).
        assert!(
            scored[1] > scored[2],
            "B (1-hop) should beat C (2-hop); got B={} C={}",
            scored[1],
            scored[2]
        );
        // A should still be the best match.
        assert!(scored[0] > scored[1]);
    }

    #[test]
    fn lightgcn_empty_input_returns_empty() {
        let g = fresh_graph();
        let scorer = LightGcnScorer::new(g as Arc<dyn GraphStore>);
        let out = scorer.score(&[1.0, 0.0], &[], &[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn lightgcn_rejects_dim_mismatch() {
        use crate::memory::cognitive::data_point::DataPoint;
        let g = fresh_graph();
        let scorer = LightGcnScorer::new(g as Arc<dyn GraphStore>);
        let node = DataPoint::entity("A", 0);
        let r = scorer.score(&[1.0, 0.0], &[node], &[vec![1.0, 0.0, 0.0]]);
        assert!(r.is_err());
    }
}

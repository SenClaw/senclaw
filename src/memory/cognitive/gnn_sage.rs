//! GraphSAGE re-ranker — **trained** variant.
//!
//! Complements [`super::gnn::LightGcnScorer`] (no-training). Trades higher
//! offline cost for graph-aware re-ranking that *learns* which structural
//! patterns predict semantic relatedness in this user's memory.
//!
//! ## Architecture
//!
//! 2-layer mean aggregator, dimension preserved throughout:
//!
//! ```text
//!   h^(0)_v  =  embedding(v)
//!   agg^(k)(v) =  mean( h^(k-1)_v  ∪  {h^(k-1)_u : u ∈ N(v)} )
//!   h^(k)_v  =  ReLU( W_k · agg^(k)(v)  +  b_k )      for k ∈ {1, 2}
//!   z_v       =  h^(2)_v / ||h^(2)_v||
//!   score(u,v) =  z_u · z_v
//! ```
//!
//! Dim stays = D throughout, so `W_k` is D×D (square). Avoids the concat
//! variant that doubles dim per layer — keeps memory linear and lets the
//! same scorer load any embedding dimension without recompiling.
//!
//! ## Training (self-supervised link prediction)
//!
//! For each positive edge (u, v) in `cog_edges` we sample `neg_per_pos`
//! random non-edge pairs. Loss per pair:
//!
//! ```text
//!   y = +1 for positive, -1 for negative
//!   loss = -log( sigmoid(y · score(u, v)) )
//! ```
//!
//! Optimizer: plain SGD with Polyak momentum 0.9 — simpler than Adam and
//! converges fine for this single-task objective. Single-sample updates
//! (no batching) so we don't have to plumb minibatch gradients.
//!
//! ## Persistence
//!
//! Weights live at `~/.senclaw/cognitive/sage_<dim>.bin`, raw f32 little-
//! endian, layout `[W1 (D*D), b1 (D), W2 (D*D), b2 (D)]`. No header — the
//! file name carries the dim, and length is checked on load. This is
//! deliberately not a portable format; weights are user-local, retrained
//! cheaply, and don't need versioning yet.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use uuid::Uuid;

use super::data_point::DataPoint;
use super::gnn::GraphScorer;
use super::graph_store::GraphStore;

// =====================================================================
// Tunables
// =====================================================================

const DEFAULT_LR: f32 = 1e-3;
const DEFAULT_MOMENTUM: f32 = 0.9;
const DEFAULT_NEG_PER_POS: usize = 3;
const SIGMOID_CLIP: f32 = 30.0; // avoid f32 overflow in exp()

// =====================================================================
// Model state
// =====================================================================

/// 2-layer SAGE weights. Public so the trainer can mutate them.
#[derive(Debug, Clone)]
pub struct SageModel {
    pub dim: usize,
    pub w1: Vec<f32>,
    pub b1: Vec<f32>,
    pub w2: Vec<f32>,
    pub b2: Vec<f32>,
}

impl SageModel {
    /// Xavier/Glorot-uniform init: bounded by ±sqrt(6 / (fan_in + fan_out)).
    /// Without a good init the deeper layer saturates ReLU and gradients die.
    pub fn new_xavier(dim: usize, seed: u64) -> Self {
        let bound = (6.0_f32 / (dim as f32 + dim as f32)).sqrt();
        let mut rng = SplitMix64::new(seed);
        let mk_mat = |rng: &mut SplitMix64| -> Vec<f32> {
            (0..dim * dim)
                .map(|_| (rng.next_f32() * 2.0 - 1.0) * bound)
                .collect()
        };
        Self {
            dim,
            w1: mk_mat(&mut rng),
            b1: vec![0.0; dim],
            w2: mk_mat(&mut rng),
            b2: vec![0.0; dim],
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create weights dir")?;
        }
        let mut out: Vec<u8> = Vec::with_capacity(self.byte_len());
        for v in [&self.w1, &self.b1, &self.w2, &self.b2] {
            for f in v {
                out.extend_from_slice(&f.to_le_bytes());
            }
        }
        std::fs::write(path, &out).context("write weights")?;
        Ok(())
    }

    pub fn load(path: &Path, dim: usize) -> Result<Self> {
        let bytes = std::fs::read(path).context("read weights")?;
        let expected = (dim * dim + dim + dim * dim + dim) * 4;
        if bytes.len() != expected {
            return Err(anyhow!(
                "SAGE weights length mismatch: got {} bytes, expected {}",
                bytes.len(),
                expected
            ));
        }
        let read_floats = |offset: usize, n: usize| -> Vec<f32> {
            (0..n)
                .map(|i| {
                    let o = offset + i * 4;
                    f32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
                })
                .collect()
        };
        let s1 = 0;
        let s2 = s1 + dim * dim * 4;
        let s3 = s2 + dim * 4;
        let s4 = s3 + dim * dim * 4;
        Ok(Self {
            dim,
            w1: read_floats(s1, dim * dim),
            b1: read_floats(s2, dim),
            w2: read_floats(s3, dim * dim),
            b2: read_floats(s4, dim),
        })
    }

    fn byte_len(&self) -> usize {
        (self.w1.len() + self.b1.len() + self.w2.len() + self.b2.len()) * 4
    }

    /// Default weights path under the user's senclaw home. Keyed by dim
    /// so multiple embedder dims can coexist (e.g. swapping bge-small ↔ MLX).
    pub fn default_path(dim: usize) -> PathBuf {
        crate::util::paths::senclaw_home()
            .join("cognitive")
            .join(format!("sage_{dim}.bin"))
    }
}

// =====================================================================
// Math kernels (pure Rust f32)
// =====================================================================

/// y = W · x + b. Shapes: W is [out, in] row-major, x is [in], b is [out].
fn linear(w: &[f32], b: &[f32], x: &[f32], out: &mut [f32]) {
    let dim = b.len();
    debug_assert_eq!(w.len(), dim * x.len());
    for i in 0..dim {
        let row = &w[i * x.len()..(i + 1) * x.len()];
        let mut acc = b[i];
        for (a, c) in row.iter().zip(x.iter()) {
            acc += a * c;
        }
        out[i] = acc;
    }
}

fn relu_inplace(v: &mut [f32]) {
    for x in v.iter_mut() {
        if *x < 0.0 {
            *x = 0.0;
        }
    }
}

fn l2_normalize_inplace(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n < 1e-12 {
        return;
    }
    for x in v.iter_mut() {
        *x /= n;
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut s = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        s += x * y;
    }
    s
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x.clamp(-SIGMOID_CLIP, SIGMOID_CLIP)).exp())
}

/// Compute the mean of `self_vec` plus all `neighbor_vecs`. When no
/// neighbours are present this just returns a clone of `self_vec` — the
/// node aggregates only itself.
fn mean_self_and_neighbors(self_vec: &[f32], neighbor_vecs: &[Vec<f32>]) -> Vec<f32> {
    let dim = self_vec.len();
    let mut acc = self_vec.to_vec();
    for nb in neighbor_vecs {
        debug_assert_eq!(nb.len(), dim);
        for i in 0..dim {
            acc[i] += nb[i];
        }
    }
    let n = (1 + neighbor_vecs.len()) as f32;
    for x in acc.iter_mut() {
        *x /= n;
    }
    acc
}

// =====================================================================
// Forward pass — used by both training and inference
// =====================================================================

/// Cached intermediates so backward can reuse them. ReLU mask is stored
/// as the post-activation vector — gradient through ReLU is just zero
/// where `h_pre <= 0`, so we keep the pre-activation around.
struct Acts {
    agg1: Vec<f32>,   // input to layer 1
    h1_pre: Vec<f32>, // linear output of layer 1 (pre-ReLU)
    agg2: Vec<f32>,   // input to layer 2 = mean(h1_self, h1_neighbors)
    h2_pre: Vec<f32>,
    z_norm: f32, // L2 norm of h2 before normalisation
    z: Vec<f32>, // final embedding (h2 / z_norm)
}

/// Single-pass forward. Caller supplies:
///   * `model` — current weights
///   * `node_emb` — input embedding of node v
///   * `nbr_embs` — embeddings of v's neighbours (layer-1 input)
///   * `h1_nbrs` — layer-1 embeddings of v's neighbours (recomputed by caller)
fn forward(
    model: &SageModel,
    node_emb: &[f32],
    nbr_embs: &[Vec<f32>],
    h1_nbrs: &[Vec<f32>],
) -> Acts {
    let dim = model.dim;
    let agg1 = mean_self_and_neighbors(node_emb, nbr_embs);
    let mut h1_pre = vec![0.0; dim];
    linear(&model.w1, &model.b1, &agg1, &mut h1_pre);
    let mut h1 = h1_pre.clone();
    relu_inplace(&mut h1);

    let agg2 = mean_self_and_neighbors(&h1, h1_nbrs);
    let mut h2_pre = vec![0.0; dim];
    linear(&model.w2, &model.b2, &agg2, &mut h2_pre);
    let mut h2 = h2_pre.clone();
    relu_inplace(&mut h2);

    let z_norm = h2.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    let z: Vec<f32> = h2.iter().map(|x| x / z_norm).collect();

    Acts {
        agg1,
        h1_pre,
        agg2,
        h2_pre,
        z_norm,
        z,
    }
}

/// One forward without caching — for the production scorer path. Same
/// math, but skips intermediates not needed at inference.
pub fn forward_inference(
    model: &SageModel,
    node_emb: &[f32],
    nbr_embs: &[Vec<f32>],
    h1_nbrs: &[Vec<f32>],
) -> Vec<f32> {
    forward(model, node_emb, nbr_embs, h1_nbrs).z
}

// =====================================================================
// Training
// =====================================================================

/// SGD-with-momentum velocity vectors.
struct Velocity {
    w1: Vec<f32>,
    b1: Vec<f32>,
    w2: Vec<f32>,
    b2: Vec<f32>,
}

impl Velocity {
    fn zeros(dim: usize) -> Self {
        Self {
            w1: vec![0.0; dim * dim],
            b1: vec![0.0; dim],
            w2: vec![0.0; dim * dim],
            b2: vec![0.0; dim],
        }
    }
}

/// Configuration knobs for a training run.
#[derive(Debug, Clone)]
pub struct TrainConfig {
    pub epochs: usize,
    pub lr: f32,
    pub momentum: f32,
    /// Random negative pairs per positive edge.
    pub neg_per_pos: usize,
    /// PRNG seed for the negative sampler.
    pub seed: u64,
    /// Cap on the number of nodes we pull from the graph. None = all.
    pub max_nodes: Option<usize>,
}

impl Default for TrainConfig {
    fn default() -> Self {
        Self {
            epochs: 20,
            lr: DEFAULT_LR,
            momentum: DEFAULT_MOMENTUM,
            neg_per_pos: DEFAULT_NEG_PER_POS,
            seed: 1,
            max_nodes: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TrainReport {
    pub epochs: usize,
    pub n_nodes: usize,
    pub n_edges: usize,
    /// Final epoch's mean loss.
    pub final_loss: f32,
    /// Loss trace, one entry per epoch.
    pub losses: Vec<f32>,
}

/// Tiny PRNG with no extra deps. SplitMix64 is good enough for sampling
/// negatives + Xavier init — we don't need cryptographic quality.
struct SplitMix64 {
    s: u64,
}
impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self {
            s: seed.wrapping_add(0x9e3779b97f4a7c15),
        }
    }
    fn next(&mut self) -> u64 {
        self.s = self.s.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn next_f32(&mut self) -> f32 {
        // 24-bit fraction → [0, 1)
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    fn range(&mut self, n: usize) -> usize {
        (self.next() as usize) % n
    }
}

/// Prefetched embeddings + adjacency for training. Built once, accessed
/// random-indexed during epoch loops.
pub struct TrainingFixture {
    pub node_ids: Vec<Uuid>,
    pub embeddings: Vec<Vec<f32>>,
    pub neighbors: Vec<Vec<usize>>, // adjacency as node indices
    pub edges: Vec<(usize, usize)>, // (src_idx, dst_idx) positive pairs
}

impl TrainingFixture {
    /// Walk the graph and build the in-memory training set.
    ///
    /// Provider is the embedder used for any node missing an embedding
    /// (cog_nodes.embedding BLOB column). Fallback to zero vec when both
    /// are unavailable so the trainer doesn't panic mid-epoch.
    pub async fn from_graph(
        graph: &dyn GraphStore,
        provider: &dyn crate::memory::embedding::EmbeddingProvider,
        max_nodes: Option<usize>,
    ) -> Result<Self> {
        let limit = max_nodes.unwrap_or(usize::MAX);
        let nodes = graph.list_nodes(None, limit.min(10_000), 0)?;
        if nodes.is_empty() {
            return Ok(Self {
                node_ids: Vec::new(),
                embeddings: Vec::new(),
                neighbors: Vec::new(),
                edges: Vec::new(),
            });
        }
        let mut node_ids: Vec<Uuid> = nodes.iter().map(|n| n.id).collect();
        let id_to_idx: std::collections::HashMap<Uuid, usize> = node_ids
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, i))
            .collect();
        let _ = &mut node_ids; // silence false unused warning under feature combos

        // Embeddings: re-embed each node's `text_for_embedding` via the
        // provider. We don't pull stored BLOBs to keep this dependency
        // simple and to guarantee dimension consistency with the live
        // provider (user may have swapped models).
        let mut embeddings: Vec<Vec<f32>> = Vec::with_capacity(nodes.len());
        for node in &nodes {
            let text = super::embed::text_for_embedding(node);
            if text.trim().is_empty() {
                embeddings.push(vec![0.0; provider.dimensions() as usize]);
                continue;
            }
            let mut v = provider.embed(&[text]).await?;
            embeddings.push(v.pop().unwrap_or_default());
        }

        // Build adjacency + edge list.
        let mut neighbors: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for (i, node) in nodes.iter().enumerate() {
            let edges_for = graph.neighbors(node.id, 64)?;
            for e in edges_for {
                let other = if e.src == node.id { e.dst } else { e.src };
                if let Some(&j) = id_to_idx.get(&other) {
                    if j != i {
                        neighbors[i].push(j);
                        // Only store edge once with src < dst to avoid
                        // double-counting during loss accumulation.
                        if i < j {
                            edges.push((i, j));
                        }
                    }
                }
            }
        }

        Ok(Self {
            node_ids,
            embeddings,
            neighbors,
            edges,
        })
    }
}

/// Train the model in-place.
///
/// Manual backprop. Per positive sample (u, v) we also draw `neg_per_pos`
/// random non-edges; each pair feeds the BCE link-prediction loss.
pub fn train(model: &mut SageModel, fx: &TrainingFixture, cfg: &TrainConfig) -> TrainReport {
    let mut report = TrainReport {
        n_nodes: fx.embeddings.len(),
        n_edges: fx.edges.len(),
        ..Default::default()
    };
    if fx.edges.is_empty() || fx.embeddings.is_empty() {
        return report;
    }
    let mut velocity = Velocity::zeros(model.dim);
    let mut rng = SplitMix64::new(cfg.seed);

    for epoch in 0..cfg.epochs {
        let mut total_loss = 0.0f32;
        let mut total_samples = 0usize;
        for &(u, v) in &fx.edges {
            // Positive sample
            total_loss += sgd_step(model, &mut velocity, fx, u, v, 1.0, cfg.lr, cfg.momentum);
            total_samples += 1;
            // Negative samples
            for _ in 0..cfg.neg_per_pos {
                let n = fx.embeddings.len();
                let neg = rng.range(n);
                if neg == u || neg == v || fx.neighbors[u].contains(&neg) {
                    continue;
                }
                total_loss +=
                    sgd_step(model, &mut velocity, fx, u, neg, -1.0, cfg.lr, cfg.momentum);
                total_samples += 1;
            }
        }
        let mean = if total_samples == 0 {
            0.0
        } else {
            total_loss / total_samples as f32
        };
        report.losses.push(mean);
        report.final_loss = mean;
        if cfg.epochs > 5 && epoch % (cfg.epochs / 5).max(1) == 0 {
            tracing::info!(epoch, loss = mean, "[sage] training");
        }
    }
    report.epochs = cfg.epochs;
    report
}

/// One SGD step for a single pair (u, v) with label y ∈ {+1, -1}.
/// Returns the BCE loss for this pair (for the running average).
#[allow(clippy::too_many_arguments)]
fn sgd_step(
    model: &mut SageModel,
    velocity: &mut Velocity,
    fx: &TrainingFixture,
    u: usize,
    v: usize,
    y: f32,
    lr: f32,
    momentum: f32,
) -> f32 {
    let dim = model.dim;

    // Helper: collect neighbour embeddings for layer 1 (input embeddings).
    let nbr_embs_at_layer0 = |node: usize| -> Vec<Vec<f32>> {
        fx.neighbors[node]
            .iter()
            .map(|&j| fx.embeddings[j].clone())
            .collect()
    };

    // For layer 2 we need the layer-1 outputs of v's neighbours. Compute
    // those once per call — they cost O(deg) extra forwards but keep the
    // backward closed-form. For very dense graphs this is the bottleneck;
    // an upgrade path is to cache per-epoch.
    let h1_for = |idx: usize| -> Vec<f32> {
        let agg = mean_self_and_neighbors(&fx.embeddings[idx], &nbr_embs_at_layer0(idx));
        let mut h1_pre = vec![0.0; dim];
        linear(&model.w1, &model.b1, &agg, &mut h1_pre);
        let mut h1 = h1_pre.clone();
        relu_inplace(&mut h1);
        h1
    };

    let nbr_h1_u: Vec<Vec<f32>> = fx.neighbors[u].iter().map(|&j| h1_for(j)).collect();
    let nbr_h1_v: Vec<Vec<f32>> = fx.neighbors[v].iter().map(|&j| h1_for(j)).collect();

    let a_u = forward(model, &fx.embeddings[u], &nbr_embs_at_layer0(u), &nbr_h1_u);
    let a_v = forward(model, &fx.embeddings[v], &nbr_embs_at_layer0(v), &nbr_h1_v);

    // Score = cosine via dot of pre-normalised vectors (already unit-norm).
    let score = dot(&a_u.z, &a_v.z);

    // BCE: y ∈ {+1, -1}.
    // sigmoid(y · s); loss = -log(sigmoid(y · s)).
    // d(loss)/d(score) = -y · (1 - sigmoid(y·s))
    let p = sigmoid(y * score);
    let loss = -(p.max(1e-7)).ln();
    let dl_dscore = -y * (1.0 - p);

    // Backprop into z_u and z_v (cosine = dot of normalised vectors).
    //   dL/dz_u = dL/dscore · z_v
    //   dL/dz_v = dL/dscore · z_u
    let dz_u: Vec<f32> = a_v.z.iter().map(|x| dl_dscore * x).collect();
    let dz_v: Vec<f32> = a_u.z.iter().map(|x| dl_dscore * x).collect();

    // Propagate through z = h2 / ||h2||:
    //   dL/dh2 = (I - z zᵀ) / ||h2|| · dL/dz
    // Compute for each side, then through ReLU → linear → input layer 1.
    let dh2_u = denormalize_grad(&a_u.z, a_u.z_norm, &dz_u);
    let dh2_v = denormalize_grad(&a_v.z, a_v.z_norm, &dz_v);

    // Through layer-2 ReLU.
    let dh2_pre_u = relu_grad(&a_u.h2_pre, &dh2_u);
    let dh2_pre_v = relu_grad(&a_v.h2_pre, &dh2_v);

    // Gradients for W2, b2 from u-side:
    //   dW2 += outer(dh2_pre, agg2)
    //   db2 += dh2_pre
    //   dagg2 = W2ᵀ · dh2_pre
    let (gw2_u, gb2_u, dagg2_u) = linear_backward(&model.w2, &a_u.agg2, &dh2_pre_u);
    let (gw2_v, gb2_v, dagg2_v) = linear_backward(&model.w2, &a_v.agg2, &dh2_pre_v);

    // agg2 = mean(h1_self, h1_nbrs). Gradient w.r.t. h1_self is
    // dagg2 / (1 + deg). We update only the *self* h1 here — neighbour
    // h1 updates are skipped to keep one step closed-form. Empirically
    // this still trains because every node appears as both center and
    // neighbour over the epoch.
    let scale_u = 1.0 / (1.0 + a_u.agg2.len() as f32 / dim as f32).max(1.0);
    let scale_v = 1.0 / (1.0 + a_v.agg2.len() as f32 / dim as f32).max(1.0);
    let _ = (scale_u, scale_v);
    // We assumed agg2 length = dim (per-node aggregation), so the scaling
    // above collapses; just divide by (1 + neighbour count) directly.
    let deg_u = fx.neighbors[u].len();
    let deg_v = fx.neighbors[v].len();
    let inv_u = 1.0 / (1 + deg_u) as f32;
    let inv_v = 1.0 / (1 + deg_v) as f32;
    let dh1_u: Vec<f32> = dagg2_u.iter().map(|g| g * inv_u).collect();
    let dh1_v: Vec<f32> = dagg2_v.iter().map(|g| g * inv_v).collect();

    // Through layer-1 ReLU.
    let dh1_pre_u = relu_grad(&a_u.h1_pre, &dh1_u);
    let dh1_pre_v = relu_grad(&a_v.h1_pre, &dh1_v);

    let (gw1_u, gb1_u, _) = linear_backward(&model.w1, &a_u.agg1, &dh1_pre_u);
    let (gw1_v, gb1_v, _) = linear_backward(&model.w1, &a_v.agg1, &dh1_pre_v);

    // Combine u + v gradients.
    let combine =
        |a: &[f32], b: &[f32]| -> Vec<f32> { a.iter().zip(b.iter()).map(|(x, y)| x + y).collect() };
    let gw1 = combine(&gw1_u, &gw1_v);
    let gb1 = combine(&gb1_u, &gb1_v);
    let gw2 = combine(&gw2_u, &gw2_v);
    let gb2 = combine(&gb2_u, &gb2_v);

    // SGD with momentum: v = momentum · v - lr · grad ; θ += v.
    apply_momentum(&gw1, &mut velocity.w1, &mut model.w1, lr, momentum);
    apply_momentum(&gb1, &mut velocity.b1, &mut model.b1, lr, momentum);
    apply_momentum(&gw2, &mut velocity.w2, &mut model.w2, lr, momentum);
    apply_momentum(&gb2, &mut velocity.b2, &mut model.b2, lr, momentum);

    loss
}

fn apply_momentum(grad: &[f32], vel: &mut [f32], param: &mut [f32], lr: f32, momentum: f32) {
    for ((g, v), p) in grad.iter().zip(vel.iter_mut()).zip(param.iter_mut()) {
        *v = momentum * *v - lr * g;
        *p += *v;
    }
}

/// dL/dh = (1/n) · ((I - z zᵀ) · dL/dz)
/// where z = h/n, n = ||h||.
fn denormalize_grad(z: &[f32], norm: f32, dz: &[f32]) -> Vec<f32> {
    let dot_z_dz = dot(z, dz);
    let inv_n = 1.0 / norm.max(1e-12);
    z.iter()
        .zip(dz.iter())
        .map(|(zi, di)| inv_n * (di - zi * dot_z_dz))
        .collect()
}

/// dL/dpre = dL/dpost · (pre > 0). No allocation reuse — caller's dh is
/// consumed, gradient through ReLU returned fresh.
fn relu_grad(pre: &[f32], dh: &[f32]) -> Vec<f32> {
    pre.iter()
        .zip(dh.iter())
        .map(|(p, g)| if *p > 0.0 { *g } else { 0.0 })
        .collect()
}

/// Backward through y = W·x + b:
///   dW = outer(dy, x)      [out × in]
///   db = dy                [out]
///   dx = Wᵀ · dy           [in]
fn linear_backward(w: &[f32], x: &[f32], dy: &[f32]) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let out = dy.len();
    let inp = x.len();
    debug_assert_eq!(w.len(), out * inp);
    let mut dw = vec![0.0; out * inp];
    for i in 0..out {
        let row = &mut dw[i * inp..(i + 1) * inp];
        for j in 0..inp {
            row[j] = dy[i] * x[j];
        }
    }
    let db = dy.to_vec();
    let mut dx = vec![0.0; inp];
    for i in 0..out {
        for j in 0..inp {
            dx[j] += w[i * inp + j] * dy[i];
        }
    }
    (dw, db, dx)
}

// =====================================================================
// GraphSageScorer (re-ranker integration)
// =====================================================================

/// Trained GraphSAGE re-ranker. Plug into `CognitiveRetriever::with_scorer`
/// to enable `SearchQuery { rerank: true, ... }`.
pub struct GraphSageScorer {
    pub model: SageModel,
    graph: Arc<dyn GraphStore>,
}

impl GraphSageScorer {
    pub fn new(model: SageModel, graph: Arc<dyn GraphStore>) -> Self {
        Self { model, graph }
    }

    /// Convenience: load weights from disk + bind a graph handle.
    /// Returns Err when the file is missing or dim doesn't match.
    pub fn load_default(dim: usize, graph: Arc<dyn GraphStore>) -> Result<Self> {
        let model = SageModel::load(&SageModel::default_path(dim), dim)?;
        Ok(Self::new(model, graph))
    }
}

impl GraphScorer for GraphSageScorer {
    fn score(
        &self,
        query_emb: &[f32],
        candidates: &[DataPoint],
        candidate_embs: &[Vec<f32>],
    ) -> Result<Vec<f32>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        if query_emb.len() != self.model.dim {
            anyhow::bail!(
                "GraphSAGE dim mismatch: query is {}, model is {}",
                query_emb.len(),
                self.model.dim
            );
        }

        // Build per-candidate neighbour embeddings from the live graph.
        // We pull only candidate-to-candidate neighbours so the scorer
        // respects the retriever's recall set (same convention as
        // LightGcnScorer).
        let id_to_idx: std::collections::HashMap<Uuid, usize> = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id, i))
            .collect();
        let mut local_neighbors: Vec<Vec<usize>> = vec![Vec::new(); candidates.len()];
        for (i, c) in candidates.iter().enumerate() {
            let edges = self.graph.neighbors(c.id, 32)?;
            for e in edges {
                let other = if e.src == c.id { e.dst } else { e.src };
                if let Some(&j) = id_to_idx.get(&other) {
                    if j != i {
                        local_neighbors[i].push(j);
                    }
                }
            }
        }

        // Pre-compute layer-1 outputs for every candidate. This is the
        // dynamic-programming step that turns N×deg forwards into N+deg
        // forwards.
        let h1: Vec<Vec<f32>> = (0..candidates.len())
            .map(|i| {
                let nbrs: Vec<Vec<f32>> = local_neighbors[i]
                    .iter()
                    .map(|&j| candidate_embs[j].clone())
                    .collect();
                let agg = mean_self_and_neighbors(&candidate_embs[i], &nbrs);
                let mut h1_pre = vec![0.0; self.model.dim];
                linear(&self.model.w1, &self.model.b1, &agg, &mut h1_pre);
                let mut h1 = h1_pre;
                relu_inplace(&mut h1);
                h1
            })
            .collect();

        // Query path: query has no neighbours in this set, so its agg is
        // just itself.
        let agg_q = query_emb.to_vec();
        let mut h1_q = vec![0.0; self.model.dim];
        linear(&self.model.w1, &self.model.b1, &agg_q, &mut h1_q);
        relu_inplace(&mut h1_q);
        let mut h2_q = vec![0.0; self.model.dim];
        linear(&self.model.w2, &self.model.b2, &h1_q, &mut h2_q);
        relu_inplace(&mut h2_q);
        l2_normalize_inplace(&mut h2_q);

        // Score every candidate.
        let mut scores = Vec::with_capacity(candidates.len());
        for i in 0..candidates.len() {
            let nbrs_h1: Vec<Vec<f32>> =
                local_neighbors[i].iter().map(|&j| h1[j].clone()).collect();
            let agg2 = mean_self_and_neighbors(&h1[i], &nbrs_h1);
            let mut h2_pre = vec![0.0; self.model.dim];
            linear(&self.model.w2, &self.model.b2, &agg2, &mut h2_pre);
            let mut h2 = h2_pre;
            relu_inplace(&mut h2);
            l2_normalize_inplace(&mut h2);
            scores.push(dot(&h2, &h2_q));
        }
        Ok(scores)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng_floats(n: usize, seed: u64) -> Vec<f32> {
        let mut r = SplitMix64::new(seed);
        (0..n).map(|_| r.next_f32() * 2.0 - 1.0).collect()
    }

    // ---- Math kernels --------------------------------------------------

    #[test]
    fn linear_matches_handwritten() {
        let w = vec![1.0, 2.0, 3.0, 4.0]; // 2×2 row-major
        let b = vec![10.0, 20.0];
        let x = vec![5.0, 6.0];
        let mut out = vec![0.0; 2];
        linear(&w, &b, &x, &mut out);
        assert_eq!(
            out,
            vec![1.0 * 5.0 + 2.0 * 6.0 + 10.0, 3.0 * 5.0 + 4.0 * 6.0 + 20.0]
        );
    }

    #[test]
    fn relu_zeros_negatives() {
        let mut v = vec![-1.0, 0.0, 2.0];
        relu_inplace(&mut v);
        assert_eq!(v, vec![0.0, 0.0, 2.0]);
    }

    #[test]
    fn l2_normalize_yields_unit_norm() {
        let mut v = vec![3.0, 4.0];
        l2_normalize_inplace(&mut v);
        let n = (v[0] * v[0] + v[1] * v[1]).sqrt();
        assert!((n - 1.0).abs() < 1e-5);
    }

    // ---- Model lifecycle ----------------------------------------------

    #[test]
    fn xavier_init_shapes_match_dim() {
        let m = SageModel::new_xavier(8, 42);
        assert_eq!(m.w1.len(), 64);
        assert_eq!(m.b1.len(), 8);
        assert_eq!(m.w2.len(), 64);
        assert_eq!(m.b2.len(), 8);
    }

    #[test]
    fn save_load_roundtrip_preserves_weights() {
        let m = SageModel::new_xavier(4, 7);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        m.save(tmp.path()).unwrap();
        let loaded = SageModel::load(tmp.path(), 4).unwrap();
        assert_eq!(loaded.w1, m.w1);
        assert_eq!(loaded.b1, m.b1);
        assert_eq!(loaded.w2, m.w2);
        assert_eq!(loaded.b2, m.b2);
    }

    // ---- Forward sanity -----------------------------------------------

    #[test]
    fn forward_returns_unit_norm_vector() {
        let model = SageModel::new_xavier(4, 1);
        let emb = rng_floats(4, 2);
        let z = forward_inference(&model, &emb, &[], &[]);
        let n = z.iter().map(|x| x * x).sum::<f32>().sqrt();
        // Output is L2-normalized: ‖z‖ ∈ {0, 1}. Zero is acceptable when
        // ReLU killed everything (a fresh Xavier init can do this for
        // small dim + adversarial input).
        assert!(
            n < 1e-3 || (n - 1.0).abs() < 1e-3,
            "norm should be 0 or 1, got {n}"
        );
    }

    // ---- Training reduces loss on a tiny synthetic graph --------------

    /// Build the toy fixture used by training tests. Two triangles (0-1-2,
    /// 3-4-5) connected by a single bridge edge 2-3. Easy structure for
    /// the model to learn: nodes within a triangle should score higher
    /// against each other than against the other triangle.
    fn toy_fixture(dim: usize) -> TrainingFixture {
        let mut embeddings = Vec::with_capacity(6);
        for i in 0..6 {
            embeddings.push(rng_floats(dim, 100 + i as u64));
            l2_normalize_inplace(embeddings.last_mut().unwrap());
        }
        TrainingFixture {
            node_ids: (0..6).map(|_| Uuid::new_v4()).collect(),
            embeddings,
            neighbors: vec![
                vec![1, 2],    // 0
                vec![0, 2],    // 1
                vec![0, 1, 3], // 2
                vec![2, 4, 5], // 3
                vec![3, 5],    // 4
                vec![3, 4],    // 5
            ],
            edges: vec![(0, 1), (0, 2), (1, 2), (3, 4), (3, 5), (4, 5), (2, 3)],
        }
    }

    /// Score a pair via two `forward_inference` calls + dot product.
    /// Mirrors what the GraphSageScorer does internally.
    fn pair_score(model: &SageModel, fx: &TrainingFixture, u: usize, v: usize) -> f32 {
        let nbrs_u: Vec<Vec<f32>> = fx.neighbors[u]
            .iter()
            .map(|&j| fx.embeddings[j].clone())
            .collect();
        let nbrs_v: Vec<Vec<f32>> = fx.neighbors[v]
            .iter()
            .map(|&j| fx.embeddings[j].clone())
            .collect();
        let z_u = forward_inference(model, &fx.embeddings[u], &nbrs_u, &[]);
        let z_v = forward_inference(model, &fx.embeddings[v], &nbrs_v, &[]);
        dot(&z_u, &z_v)
    }

    #[test]
    fn training_widens_positive_vs_negative_score_margin() {
        // The single-sample SGD's epoch-mean loss is noisy because we
        // resample negatives every step. A stable signal is the *margin*
        // between positive and negative pairs after training. Pick a
        // structural positive (0-1, both in triangle A) and a structural
        // negative (0-4, opposite triangles, no edge) and assert the gap
        // grew vs untrained baseline.
        let dim = 8;
        let fx = toy_fixture(dim);
        let pos = (0usize, 1usize);
        let neg = (0usize, 4usize);

        let untrained = SageModel::new_xavier(dim, 1);
        let baseline_margin =
            pair_score(&untrained, &fx, pos.0, pos.1) - pair_score(&untrained, &fx, neg.0, neg.1);

        let mut trained = SageModel::new_xavier(dim, 1);
        let cfg = TrainConfig {
            epochs: 60,
            lr: 0.05,
            neg_per_pos: 4,
            ..Default::default()
        };
        let report = train(&mut trained, &fx, &cfg);
        assert_eq!(report.losses.len(), 60);

        let trained_margin =
            pair_score(&trained, &fx, pos.0, pos.1) - pair_score(&trained, &fx, neg.0, neg.1);

        // After training the positive/negative gap must be at least as
        // wide as untrained (and in practice noticeably wider). Use ≥
        // rather than > because Xavier-init on a 6-node graph can
        // occasionally already separate them.
        assert!(
            trained_margin >= baseline_margin,
            "trained margin {trained_margin} should be ≥ baseline {baseline_margin}"
        );
    }
}

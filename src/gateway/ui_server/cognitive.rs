//! Cognitive memory HTTP API. Mounted under `/api/cognitive/*`.
//!
//! Endpoints:
//!   * `GET    /api/cognitive/stats`
//!   * `GET    /api/cognitive/nodes?kind=&limit=&offset=`
//!   * `GET    /api/cognitive/node/:id`
//!   * `GET    /api/cognitive/edges?node=&limit=`
//!   * `GET    /api/cognitive/decay-log?limit=`
//!   * `POST   /api/cognitive/search       { query, mode, limit, hops }`
//!   * `DELETE /api/cognitive/node/:id`
//!
//! All handlers require the daemon to have booted the cognitive system
//! (i.e. an embedding provider is configured). When dormant, every endpoint
//! returns HTTP 503 with a clear message instead of pretending to work.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use axum_extra::extract::Multipart;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::memory::cognitive::{
    self, CognitiveSystem, DataPoint, RelationshipEdge, SearchHit, SearchQuery, SearchType,
};

use super::core::{AppError, UiState};

// =====================================================================
// Helpers
// =====================================================================

fn require_system() -> Result<Arc<CognitiveSystem>, AppError> {
    cognitive::try_get_instance().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Cognitive system is dormant — configure an embedding provider \
             (SENCLAW_MEMORY_EMBEDDING_PROVIDER) and restart the daemon."
                .to_owned(),
        )
    })
}

fn parse_uuid(raw: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(raw)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, format!("invalid uuid: {e}")))
}

/// Map a wire `mode` string to a [`SearchType`]. Shared by search + recall.
/// Unknown / missing modes default to graph completion.
pub(crate) fn search_type_from_mode(mode: Option<&str>) -> SearchType {
    match mode.unwrap_or("graph") {
        "chunks" => SearchType::Chunks,
        "triplet" => SearchType::Triplet,
        "spreading" => SearchType::SpreadingActivation,
        "fts" => SearchType::Fts,
        "hybrid" => SearchType::Hybrid,
        "temporal" => SearchType::Temporal,
        _ => SearchType::GraphCompletion,
    }
}

/// Read the optional `asOf` field off a request body. `Err` on an
/// unparseable value — quietly answering about *now* when the caller asked
/// about a past date is the one failure mode temporal retrieval exists to
/// prevent.
fn as_of_from_body(raw: Option<&str>) -> Result<Option<i64>, AppError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(v) => cognitive::parse_as_of(v).map(Some).ok_or_else(|| {
            AppError(
                StatusCode::BAD_REQUEST,
                format!("could not read asOf {v:?} — use 2026-07-31, 2026-07-31T09:00:00Z or unix seconds"),
            )
        }),
    }
}

// =====================================================================
// Wire shapes — kept distinct from the storage types so we can tighten the
// API without affecting the storage layer.
// =====================================================================

#[derive(Debug, Clone, Serialize)]
pub struct NodeView {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub summary: String,
    pub salience: f32,
    pub mention_count: u32,
    pub created_at: i64,
    pub last_seen_at: i64,
}

impl From<DataPoint> for NodeView {
    fn from(n: DataPoint) -> Self {
        Self {
            id: n.id.to_string(),
            kind: n.kind.as_str().to_owned(),
            name: n.name,
            summary: n.summary,
            salience: n.salience,
            mention_count: n.mention_count,
            created_at: n.created_at,
            last_seen_at: n.last_seen_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeView {
    pub src: String,
    pub dst: String,
    pub predicate: String,
    pub strength: f32,
    pub tier: u8,
    pub ltp_status: u8,
    pub activation_count: u32,
    pub last_activated: i64,
    /// True for edges materialised by the associative-inference maintenance
    /// step (co-occurrence reasoning) rather than LLM triplet extraction.
    /// The UI styles these differently (dashed) so users can tell a derived
    /// guess from an extracted fact.
    pub inferred: bool,
    /// World time the fact was asserted from.
    #[serde(rename = "validFrom")]
    pub valid_from: i64,
    /// Set once a later fact replaced this one — the UI strikes those through
    /// instead of showing them as equal truth.
    #[serde(rename = "validTo")]
    pub valid_to: Option<i64>,
    /// Set when decay put the edge to sleep. Orthogonal to `validTo`:
    /// dormant is not false.
    #[serde(rename = "archivedAt")]
    pub archived_at: Option<i64>,
}

impl From<RelationshipEdge> for EdgeView {
    fn from(e: RelationshipEdge) -> Self {
        // Inference marks its edges two ways: predicate ASSOCIATED_WITH and
        // props_json.inferred = true. Check both so the flag survives even
        // if a future predicate reuses the label.
        let inferred = e.predicate == "ASSOCIATED_WITH"
            || e.props
                .get("inferred")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        Self {
            src: e.src.to_string(),
            dst: e.dst.to_string(),
            predicate: e.predicate,
            strength: e.strength,
            tier: e.tier as u8,
            ltp_status: e.ltp_status as u8,
            activation_count: e.activation_count,
            last_activated: e.last_activated,
            inferred,
            valid_from: e.valid_from,
            valid_to: e.valid_to,
            archived_at: e.archived_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HitView {
    pub node: NodeView,
    pub score: f32,
    pub path_len: usize,
}

impl From<SearchHit> for HitView {
    fn from(h: SearchHit) -> Self {
        Self {
            path_len: h.path.len(),
            node: h.node.into(),
            score: h.score,
        }
    }
}

// =====================================================================
// GET /api/cognitive/stats
// =====================================================================

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub edges: usize,
    pub nodes_total: usize,
    pub nodes_by_kind: Vec<(String, usize)>,
}

pub(crate) async fn cognitive_stats(
    State(_s): State<Arc<UiState>>,
) -> Result<Json<StatsResponse>, AppError> {
    let sys = require_system()?;
    let edges = sys
        .stats()
        .map(|s| s.edges)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let nodes_total = sys
        .graph
        .count_nodes(None)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let kinds = ["entity", "chunk", "summary", "custom"];
    let mut by_kind = Vec::with_capacity(kinds.len());
    for k in &kinds {
        let n = sys
            .graph
            .count_nodes(Some(k))
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if n > 0 {
            by_kind.push((k.to_string(), n));
        }
    }
    Ok(Json(StatsResponse {
        edges,
        nodes_total,
        nodes_by_kind: by_kind,
    }))
}

// =====================================================================
// GET /api/cognitive/nodes
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct ListNodesQuery {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

fn default_limit() -> usize {
    50
}

#[derive(Debug, Serialize)]
pub struct ListNodesResponse {
    pub total: usize,
    pub nodes: Vec<NodeView>,
}

pub(crate) async fn cognitive_list_nodes(
    State(_s): State<Arc<UiState>>,
    Query(q): Query<ListNodesQuery>,
) -> Result<Json<ListNodesResponse>, AppError> {
    let sys = require_system()?;
    let kind = q.kind.as_deref();
    let limit = q.limit.clamp(1, 500);
    let total = sys
        .graph
        .count_nodes(kind)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let nodes = sys
        .graph
        .list_nodes(kind, limit, q.offset)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ListNodesResponse {
        total,
        nodes: nodes.into_iter().map(NodeView::from).collect(),
    }))
}

// =====================================================================
// GET /api/cognitive/node/:id
// =====================================================================

#[derive(Debug, Serialize)]
pub struct NodeDetailResponse {
    pub node: NodeView,
    pub edges: Vec<EdgeView>,
}

pub(crate) async fn cognitive_get_node(
    State(_s): State<Arc<UiState>>,
    Path(id): Path<String>,
) -> Result<Json<NodeDetailResponse>, AppError> {
    let sys = require_system()?;
    let uuid = parse_uuid(&id)?;
    let node = sys
        .graph
        .get_node(uuid)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "node not found".into()))?;
    let edges = sys
        .graph
        .neighbors(uuid, 64)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(NodeDetailResponse {
        node: node.into(),
        edges: edges.into_iter().map(EdgeView::from).collect(),
    }))
}

// =====================================================================
// GET /api/cognitive/decay-log
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct DecayLogQuery {
    #[serde(default = "default_decay_limit")]
    pub limit: usize,
}

fn default_decay_limit() -> usize {
    20
}

pub(crate) async fn cognitive_decay_log(
    State(_s): State<Arc<UiState>>,
    Query(q): Query<DecayLogQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    let limit = q.limit.clamp(1, 200);
    let rows = sys
        .graph
        .recent_decay_runs(limit)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "runs": rows })))
}

// =====================================================================
// GET /api/cognitive/top-nodes
// =====================================================================
//
// Used by the Graph Explorer to surface "interesting" seed candidates —
// the user picks a name from this list (or accepts the default selection)
// and the UI calls /sample to actually render the subgraph.
//
// Cheap query: degree aggregate over `cog_edges`, no embeddings needed.

#[derive(Debug, Deserialize)]
pub struct TopNodesQuery {
    #[serde(default = "default_top_limit")]
    pub limit: usize,
    /// Restrict to one knowledge space (custom scope id). None = global,
    /// ordered by degree; with a space, ordered by recency within the space.
    #[serde(default)]
    pub space: Option<String>,
}
fn default_top_limit() -> usize {
    20
}

#[derive(Debug, Serialize)]
pub struct TopNodeView {
    pub node: NodeView,
    pub degree: usize,
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    /// Entity name (case-insensitive) or node UUID.
    pub subject: String,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default = "default_history_limit")]
    pub limit: usize,
}

fn default_history_limit() -> usize {
    100
}

/// GET /api/cognitive/history — every value a subject's predicate has held.
///
/// The read side of supersession: when a fact changes, the old edge keeps its
/// row with `validTo` stamped, so the whole timeline is always available
/// rather than being overwritten.
pub(crate) async fn cognitive_history(
    State(_s): State<Arc<UiState>>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    let subject = q.subject.trim();
    if subject.is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "subject is empty".into()));
    }
    // Accept a UUID as well as a name — the graph view links by id.
    let node = match uuid::Uuid::parse_str(subject) {
        Ok(id) => sys.graph.get_node(id),
        Err(_) => sys.graph.find_entity_by_name(subject),
    }
    .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let Some(node) = node else {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("no node matching {subject:?}"),
        ));
    };

    let rows = sys
        .graph
        .edge_history(node.id, q.predicate.as_deref(), q.limit.clamp(1, 500))
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let facts: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|e| {
            let object = sys
                .graph
                .get_node(e.dst)
                .ok()
                .flatten()
                .map(|n| if n.name.is_empty() { n.summary } else { n.name })
                .unwrap_or_default();
            serde_json::json!({
                "predicate": e.predicate,
                "object": object,
                "objectId": e.dst.to_string(),
                "validFrom": e.valid_from,
                "validTo": e.valid_to,
                "current": e.is_current(),
                "archivedAt": e.archived_at,
                "invalidatedBy": e.invalidated_by.map(|u| u.to_string()),
                "strength": e.strength,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "subject": { "id": node.id.to_string(), "name": node.name },
        "facts": facts,
    })))
}

/// GET /api/cognitive/predicates — the cardinality registry that decides
/// which new facts supersede old ones.
pub(crate) async fn cognitive_predicates(
    State(_s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    let rows = sys
        .graph
        .list_predicate_meta()
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "predicates": rows })))
}

#[derive(Debug, Deserialize)]
pub struct PredicateUpdateBody {
    pub predicate: String,
    /// "single" (a new object supersedes the old) or "multi".
    pub cardinality: String,
}

/// POST /api/cognitive/predicates — correct a classification by hand.
/// Marked `source = "user"` so a future re-seed never overwrites it.
pub(crate) async fn cognitive_set_predicate(
    State(_s): State<Arc<UiState>>,
    Json(body): Json<PredicateUpdateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    let card = match body.cardinality.trim().to_ascii_lowercase().as_str() {
        "single" => cognitive::Cardinality::Single,
        "multi" => cognitive::Cardinality::Multi,
        other => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("cardinality must be \"single\" or \"multi\", got {other:?}"),
            ))
        }
    };
    sys.graph
        .set_predicate_cardinality(body.predicate.trim(), card, "user")
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "predicate": body.predicate.trim(),
        "cardinality": card.as_str(),
    })))
}

pub(crate) async fn cognitive_top_nodes(
    State(_s): State<Arc<UiState>>,
    Query(q): Query<TopNodesQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    let limit = q.limit.clamp(1, 200);
    if let Some(space) = q.space.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let nodes = sys
            .graph
            .nodes_in_set(&cognitive::NodeSet::space(space), limit)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let views: Vec<TopNodeView> = nodes
            .into_iter()
            .map(|n| TopNodeView {
                node: n.into(),
                degree: 0,
            })
            .collect();
        return Ok(Json(serde_json::json!({ "nodes": views })));
    }
    let rows = sys
        .graph
        .top_nodes_by_degree(limit)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let views: Vec<TopNodeView> = rows
        .into_iter()
        .map(|r| TopNodeView {
            node: r.node.into(),
            degree: r.degree,
        })
        .collect();
    Ok(Json(serde_json::json!({ "nodes": views })))
}

// =====================================================================
// GET /api/cognitive/spaces
// =====================================================================
//
// Registry of knowledge spaces/scopes with member counts. The UI space
// picker uses this to let the user switch between independent memories
// (e.g. each AI-Office staff member's private space).

pub(crate) async fn cognitive_spaces(
    State(_s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    let sets = sys
        .graph
        .list_node_sets()
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "spaces": sets })))
}

// =====================================================================
// GET /api/cognitive/sample
// =====================================================================
//
// Returns a merged subgraph reachable from the top-K most-connected
// nodes. Use this as the Graph Explorer's "default sample" on mount.
// `seed_count` chooses how many top-degree nodes to use as BFS seeds;
// `hops` and `limit` clamp the resulting size like /subgraph does.
//
// Multi-seed merge happens server-side so the UI gets a single payload
// with deduplicated nodes/edges — saves the client from N round-trips +
// client-side union logic.

#[derive(Debug, Deserialize)]
pub struct SampleQuery {
    #[serde(default = "default_seed_count")]
    pub seed_count: usize,
    #[serde(default = "default_sample_hops")]
    pub hops: u8,
    #[serde(default = "default_sample_limit")]
    pub limit: usize,
}
fn default_seed_count() -> usize {
    5
}
fn default_sample_hops() -> u8 {
    2
}
fn default_sample_limit() -> usize {
    150
}

pub(crate) async fn cognitive_sample(
    State(_s): State<Arc<UiState>>,
    Query(q): Query<SampleQuery>,
) -> Result<Json<SubgraphResponse>, AppError> {
    let sys = require_system()?;
    let seed_count = q.seed_count.clamp(1, 20);
    let hops = q.hops.clamp(1, 5);
    let limit = q.limit.clamp(2, 500);

    // Pick top-degree seeds.
    let seeds = sys
        .graph
        .top_nodes_by_degree(seed_count)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if seeds.is_empty() {
        return Ok(Json(SubgraphResponse {
            nodes: Vec::new(),
            edges: Vec::new(),
            truncated: false,
        }));
    }

    // Multi-seed BFS, dedup as we go. Same skeleton as `cognitive_subgraph`
    // but with N starting points.
    use std::collections::{HashMap, HashSet, VecDeque};
    let mut visited: HashMap<Uuid, DataPoint> = HashMap::new();
    let mut frontier: VecDeque<Uuid> = VecDeque::new();
    let mut next_frontier: VecDeque<Uuid> = VecDeque::new();
    let mut edges: Vec<RelationshipEdge> = Vec::new();
    let mut seen_edges: HashSet<(Uuid, Uuid, String)> = HashSet::new();

    for s in seeds {
        if visited.len() >= limit {
            break;
        }
        visited.insert(s.node.id, s.node.clone());
        frontier.push_back(s.node.id);
    }

    let mut truncated = false;
    for _ in 0..hops {
        while let Some(nid) = frontier.pop_front() {
            if visited.len() >= limit {
                truncated = true;
                break;
            }
            let nbrs = sys
                .graph
                .neighbors(nid, 32)
                .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            for edge in nbrs {
                let other = if edge.src == nid { edge.dst } else { edge.src };
                if !visited.contains_key(&other) {
                    if visited.len() >= limit {
                        truncated = true;
                        continue;
                    }
                    if let Ok(Some(n)) = sys.graph.get_node(other) {
                        visited.insert(n.id, n);
                        next_frontier.push_back(other);
                    }
                }
                let key = (edge.src, edge.dst, edge.predicate.clone());
                if !seen_edges.contains(&key) {
                    seen_edges.insert(key);
                    edges.push(edge);
                }
            }
        }
        std::mem::swap(&mut frontier, &mut next_frontier);
        next_frontier.clear();
        if visited.len() >= limit || frontier.is_empty() {
            break;
        }
    }

    // Drop edges with endpoints outside the visited set (defensive).
    let edges: Vec<EdgeView> = edges
        .into_iter()
        .filter(|e| visited.contains_key(&e.src) && visited.contains_key(&e.dst))
        .map(EdgeView::from)
        .collect();
    let nodes: Vec<NodeView> = visited.into_values().map(NodeView::from).collect();

    Ok(Json(SubgraphResponse {
        nodes,
        edges,
        truncated,
    }))
}

// =====================================================================
// GET /api/cognitive/subgraph
// =====================================================================
//
// Extracts a BFS subgraph rooted at `seed`. Used by the force-directed
// graph visualization in the UI — cap on size keeps the client render
// cheap. Edges only between *visited* nodes so the response is a
// self-contained, layout-ready graph.

#[derive(Debug, Deserialize)]
pub struct SubgraphQuery {
    pub seed: String,
    #[serde(default = "default_subgraph_hops")]
    pub hops: u8,
    #[serde(default = "default_subgraph_limit")]
    pub limit: usize,
}

fn default_subgraph_hops() -> u8 {
    2
}
fn default_subgraph_limit() -> usize {
    100
}

#[derive(Debug, Serialize)]
pub struct SubgraphResponse {
    pub nodes: Vec<NodeView>,
    pub edges: Vec<EdgeView>,
    /// True if BFS hit `limit` before exhausting reachable nodes — UI can
    /// show a "results truncated" hint.
    pub truncated: bool,
}

pub(crate) async fn cognitive_subgraph(
    State(_s): State<Arc<UiState>>,
    Query(q): Query<SubgraphQuery>,
) -> Result<Json<SubgraphResponse>, AppError> {
    let sys = require_system()?;
    let seed = parse_uuid(&q.seed)?;
    let hops = q.hops.clamp(1, 5);
    let limit = q.limit.clamp(2, 200);

    // BFS from `seed`. Visit nodes layer-by-layer, stopping when we hit
    // `limit` *visited* (so layer boundaries are respected even when we
    // truncate). Edges are kept only when both endpoints are visited so
    // the response is self-contained.
    use std::collections::{HashMap, HashSet, VecDeque};

    let mut visited: HashMap<Uuid, DataPoint> = HashMap::new();
    let mut frontier: VecDeque<Uuid> = VecDeque::new();
    let mut next_frontier: VecDeque<Uuid> = VecDeque::new();
    let mut edges: Vec<RelationshipEdge> = Vec::new();
    let mut seen_edges: HashSet<(Uuid, Uuid, String)> = HashSet::new();

    match sys.graph.get_node(seed) {
        Ok(Some(node)) => {
            visited.insert(node.id, node);
            frontier.push_back(seed);
        }
        Ok(None) => return Err(AppError(StatusCode::NOT_FOUND, "seed not found".into())),
        Err(e) => return Err(AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }

    let mut truncated = false;
    for _ in 0..hops {
        while let Some(nid) = frontier.pop_front() {
            if visited.len() >= limit {
                truncated = true;
                break;
            }
            let nbrs = sys
                .graph
                .neighbors(nid, 32)
                .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            for edge in nbrs {
                let other = if edge.src == nid { edge.dst } else { edge.src };
                if !visited.contains_key(&other) {
                    if visited.len() >= limit {
                        truncated = true;
                        continue;
                    }
                    if let Ok(Some(n)) = sys.graph.get_node(other) {
                        visited.insert(n.id, n);
                        next_frontier.push_back(other);
                    }
                }
                let key = (edge.src, edge.dst, edge.predicate.clone());
                if !seen_edges.contains(&key) {
                    seen_edges.insert(key);
                    edges.push(edge);
                }
            }
        }
        std::mem::swap(&mut frontier, &mut next_frontier);
        next_frontier.clear();
        if visited.len() >= limit || frontier.is_empty() {
            break;
        }
    }

    // Drop edges with endpoints outside the visited set (defensive).
    let edges: Vec<EdgeView> = edges
        .into_iter()
        .filter(|e| visited.contains_key(&e.src) && visited.contains_key(&e.dst))
        .map(EdgeView::from)
        .collect();
    let nodes: Vec<NodeView> = visited.into_values().map(NodeView::from).collect();

    Ok(Json(SubgraphResponse {
        nodes,
        edges,
        truncated,
    }))
}

// =====================================================================
// GET /api/cognitive/full-graph
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct FullGraphQuery {
    #[serde(default = "default_full_node_limit")]
    pub node_limit: usize,
    #[serde(default = "default_full_edge_limit")]
    pub edge_limit: usize,
    #[serde(default)]
    pub include_chunks: bool,
    #[serde(default)]
    pub connected_only: bool,
    /// Restrict to one knowledge space (custom scope id). None = whole graph.
    /// Same encoding as /top-nodes and /search: the bare space id string.
    #[serde(default)]
    pub space: Option<String>,
}
fn default_full_node_limit() -> usize {
    500
}
fn default_full_edge_limit() -> usize {
    2000
}

pub(crate) async fn cognitive_full_graph(
    State(_s): State<Arc<UiState>>,
    Query(q): Query<FullGraphQuery>,
) -> Result<Json<SubgraphResponse>, AppError> {
    let sys = require_system()?;
    let node_limit = q.node_limit.clamp(10, 5000);
    let edge_limit = q.edge_limit.clamp(10, 10000);

    let space = q.space.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (nodes, edges) = if let Some(space) = space {
        // Space-scoped view: the induced subgraph of one knowledge space —
        // members of the space plus only the edges that stay inside it.
        let set = cognitive::NodeSet::space(space);
        let mut nodes = sys
            .graph
            .nodes_in_set(&set, node_limit)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if !q.include_chunks {
            nodes.retain(|n| n.kind != cognitive::NodeKind::Chunk);
        }
        let member_ids: std::collections::HashSet<Uuid> = nodes.iter().map(|n| n.id).collect();
        let edges: Vec<RelationshipEdge> = sys
            .graph
            .edges_within_set(&set, edge_limit)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .into_iter()
            // Drop edges touching members outside the (possibly truncated /
            // chunk-filtered) node list so the payload stays self-contained.
            .filter(|e| member_ids.contains(&e.src) && member_ids.contains(&e.dst))
            .collect();
        (nodes, edges)
    } else {
        sys.graph
            .full_graph(node_limit, edge_limit, q.include_chunks)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    let (nodes, edges) = if q.connected_only {
        use std::collections::HashSet;
        let connected: HashSet<Uuid> = edges.iter().flat_map(|e| [e.src, e.dst]).collect();
        let nodes: Vec<_> = nodes
            .into_iter()
            .filter(|n| connected.contains(&n.id))
            .collect();
        (nodes, edges)
    } else {
        (nodes, edges)
    };

    let n_view: Vec<NodeView> = nodes.into_iter().map(NodeView::from).collect();
    let e_view: Vec<EdgeView> = edges.into_iter().map(EdgeView::from).collect();
    let truncated = n_view.len() >= node_limit || e_view.len() >= edge_limit;

    Ok(Json(SubgraphResponse {
        nodes: n_view,
        edges: e_view,
        truncated,
    }))
}

// =====================================================================
// POST /api/cognitive/search
// =====================================================================

#[derive(Debug, Deserialize)]
pub struct SearchBody {
    pub query: String,
    /// chunks | triplet | graph | spreading | fts | hybrid. Default: graph.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    #[serde(default = "default_hops")]
    pub hops: u8,
    #[serde(default)]
    pub rerank: bool,
    /// Restrict to one knowledge space (custom scope id). None = global.
    #[serde(default)]
    pub space: Option<String>,
    /// Answer as of this moment instead of now ("2026-07-31", RFC 3339, or
    /// unix seconds). Omit for current facts.
    #[serde(default, rename = "asOf")]
    pub as_of: Option<String>,
}

fn default_search_limit() -> usize {
    10
}
fn default_hops() -> u8 {
    2
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub hits: Vec<HitView>,
}

pub(crate) async fn cognitive_search(
    State(_s): State<Arc<UiState>>,
    Json(body): Json<SearchBody>,
) -> Result<Json<SearchResponse>, AppError> {
    let sys = require_system()?;
    let query_type = search_type_from_mode(body.mode.as_deref());
    let limit = body.limit.clamp(1, 50);
    let mut q = SearchQuery::chunks(body.query, limit);
    q.query_type = query_type;
    q.hops = body.hops.clamp(1, 6);
    q.rerank = body.rerank;
    q.decay_per_hop = 0.6;
    q.as_of = as_of_from_body(body.as_of.as_deref())?;
    if let Some(space) = body
        .space
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        q.node_sets = vec![cognitive::NodeSet::space(space)];
    }

    let hits = sys
        .search(&q)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(SearchResponse {
        hits: hits.into_iter().map(HitView::from).collect(),
    }))
}

// =====================================================================
// POST /api/cognitive/node/:id/re-extract
// =====================================================================
//
// Re-run cognify on an existing chunk's text. Use when:
//   * chunks were saved while the cognitive LLM was dormant (no edges)
//   * the LLM prompt was changed and you want to back-fill old chunks
//   * a chunk's triplets were wrong / out-of-date
//
// Only valid on chunk-kind nodes — entities have no source text.

pub(crate) async fn cognitive_re_extract(
    State(_s): State<Arc<UiState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    let uuid = parse_uuid(&id)?;
    let node = sys
        .graph
        .get_node(uuid)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "node not found".into()))?;

    // Only chunk nodes carry the raw text to re-extract from. Entity /
    // summary nodes are derived artefacts — their existence already
    // implies extraction happened.
    if node.kind != crate::memory::cognitive::NodeKind::Chunk {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!(
                "re-extract only valid on chunk nodes; this node is `{}`",
                node.kind.as_str()
            ),
        ));
    }
    if node.summary.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "chunk has no text to extract".into(),
        ));
    }

    // Reset the state machine first: a chunk marked `Done` short-circuits
    // at cognify's dedupe gate, so without this an explicit re-extract on
    // an already-processed chunk (e.g. one whose edges have since decayed
    // away) is a silent no-op.
    let _ = sys.graph.set_extraction_state(
        uuid,
        crate::memory::cognitive::ExtractionState::Pending,
        chrono::Utc::now().timestamp(),
    );

    // Forward into the same cognify pipeline used by CogAdd. Content-hash
    // dedupe will reuse the existing chunk node — we only get new edges.
    // Recovered entities inherit the chunk's own knowledge spaces.
    let opts = crate::memory::cognitive::CognifyOptions {
        node_sets: sys.graph.sets_of_node(uuid).unwrap_or_default(),
        ..Default::default()
    };
    let report = sys
        .cognify(&node.summary, "re-extract", &opts)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({
        "chunks_deduped": report.chunks_deduped,
        "entities_added": report.entities_added,
        "entities_reused": report.entities_reused,
        "edges_added": report.edges_added,
        "edges_strengthened": report.edges_strengthened,
        "llm_skipped": report.llm_skipped,
    })))
}

// =====================================================================
// POST /api/cognitive/re-extract-pending  { limit? }
// =====================================================================
//
// Bulk backfill: re-run triplet extraction on every chunk still marked
// `Pending` / `SkippedNoLlm` — the rows accumulated while the cognitive
// LLM was dormant or misconfigured (e.g. an SSE-only gateway). Also
// rescues `Done` chunks whose extracted edges have all decayed away
// (pre-L2-tier era): those get their state reset to `Pending` first so
// the same scan picks them up and the stored text yields its facts
// again. Runs in a background task (one chunk at a time through the
// cognify semaphore) and returns the queued count immediately so the UI
// doesn't block on N LLM calls; progress shows up as the node/edge
// stats grow.

pub(crate) async fn cognitive_re_extract_pending(
    State(_s): State<Arc<UiState>>,
    body: Option<Json<serde_json::Value>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    let limit = body
        .as_ref()
        .and_then(|b| b.get("limit"))
        .and_then(|v| v.as_u64())
        .unwrap_or(500) as usize;

    // Rescue pass: `Done` chunks that lost every outgoing edge go back to
    // `Pending` so the scan below treats them like never-extracted rows.
    let reset = sys
        .graph
        .reset_orphan_done_chunks()
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if reset > 0 {
        tracing::info!(
            reset,
            "[cognitive] backfill: reset Done-but-edgeless chunks to Pending"
        );
    }

    // Page through all chunks and keep the never-extracted ones.
    use crate::memory::cognitive::ExtractionState as S;
    let mut pending = Vec::new();
    let mut offset = 0usize;
    'scan: loop {
        let batch = sys
            .graph
            .list_nodes(Some("chunk"), 500, offset)
            .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if batch.is_empty() {
            break;
        }
        offset += batch.len();
        for n in batch {
            if matches!(n.extraction_state, S::Pending | S::SkippedNoLlm)
                && !n.summary.trim().is_empty()
            {
                // Recovered entities must land in the chunk's own knowledge
                // spaces, not the unscoped default.
                let sets = sys.graph.sets_of_node(n.id).unwrap_or_default();
                pending.push((n.summary, sets));
                if pending.len() >= limit {
                    break 'scan;
                }
            }
        }
    }

    let queued = pending.len();
    tokio::spawn(async move {
        let Some(sys) = crate::memory::cognitive::try_get_instance() else {
            return;
        };
        let mut entities = 0usize;
        let mut edges = 0usize;
        let mut skipped = 0usize;
        for (text, sets) in pending {
            let opts = crate::memory::cognitive::CognifyOptions {
                node_sets: sets,
                ..Default::default()
            };
            match sys.cognify(&text, "re-extract", &opts).await {
                Ok(r) => {
                    entities += r.entities_added;
                    edges += r.edges_added;
                    if r.llm_skipped {
                        skipped += 1;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "[cognitive] backfill cognify failed"),
            }
        }
        tracing::info!(
            queued,
            entities_added = entities,
            edges_added = edges,
            llm_skipped = skipped,
            "[cognitive] pending re-extract backfill finished"
        );
    });

    Ok(Json(
        serde_json::json!({ "queued": queued, "reset": reset }),
    ))
}

// =====================================================================
// POST /api/cognitive/cleanup
// =====================================================================
//
// One-shot bulk-cleanup of junk (envelope/markup-heavy chunks, symbol-only
// and ungrounded entities, orphan entities and type nodes — see
// `GraphStore::cleanup_junk` for the full category list). Triggered from
// the DataPoints view header so users don't have to forget rows one at a
// time.

pub(crate) async fn cognitive_cleanup(
    State(_s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    let report = sys
        .graph
        .cleanup_junk()
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Full per-category counts + a total the UI can show as one line.
    let mut body = serde_json::to_value(&report)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    body["total_removed"] = serde_json::json!(report.total_removed());
    Ok(Json(body))
}

// =====================================================================
// POST /api/cognitive/maintenance
// =====================================================================
//
// Full maintenance sweep: cleanup_junk + merge_duplicate_entities. This
// is the same routine the background ticker runs on a schedule; the
// endpoint lets users trigger it on demand from the Settings UI.

pub(crate) async fn cognitive_maintenance(
    State(_s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    let graph = Arc::clone(&sys.graph);
    let report = tokio::task::spawn_blocking(move || cognitive::run_maintenance(&*graph))
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({
        "envelope_chunks_removed": report.cleanup.envelope_chunks_removed,
        "markup_chunks_removed": report.cleanup.markup_chunks_removed,
        "junk_entities_removed": report.cleanup.junk_entities_removed,
        "orphan_entities_removed": report.cleanup.orphan_entities_removed,
        "typeonly_entities_removed": report.cleanup.typeonly_entities_removed,
        "orphan_type_nodes_removed": report.cleanup.orphan_type_nodes_removed,
        "cleanup_total_removed": report.cleanup.total_removed(),
        "groups_merged": report.merge.groups_merged,
        "entities_merged": report.merge.entities_merged,
        "edges_redirected": report.merge.edges_redirected,
        "aliases_merged": report.alias_merge.entities_merged,
        "alias_edges_redirected": report.alias_merge.edges_redirected,
        "associations_inferred": report.inference.associations_created,
        "association_candidates": report.inference.candidates_examined,
        "duration_ms": report.duration_ms,
    })))
}

// =====================================================================
// DELETE /api/cognitive/node/:id
// =====================================================================

pub(crate) async fn cognitive_forget(
    State(_s): State<Arc<UiState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    let uuid = parse_uuid(&id)?;
    sys.graph
        .delete_node(uuid)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = sys.vector.delete(uuid); // best effort
    Ok(Json(serde_json::json!({ "forgotten": id })))
}

// =====================================================================
// Ingestion: POST /api/cognitive/add (text) + /api/cognitive/upload (file)
// =====================================================================
//
// The Web UI's path into the cognify pipeline. Knowledge bases map onto
// NodeSet tags (the KB→NodeSet decision): every upload is tagged
// `global:default_memory` plus any caller-supplied tags, so the same graph
// can be partitioned without a parallel multi-KB table system.

/// Build the node-set tags for a UI ingestion. Always includes the default
/// memory scope; extra non-empty tags become additional `global` scopes and
/// an optional knowledge `space` adds its custom scope so the item is
/// recallable from that space in isolation.
fn ingest_node_sets(tags: &[String], space: Option<&str>) -> Vec<cognitive::NodeSet> {
    let mut sets = vec![cognitive::NodeSet::global("default_memory")];
    if let Some(space) = space.map(str::trim).filter(|s| !s.is_empty()) {
        sets.push(cognitive::NodeSet::space(space));
    }
    for t in tags {
        let t = t.trim();
        if !t.is_empty() && t != "default_memory" {
            sets.push(cognitive::NodeSet::global(t));
        }
    }
    sets
}

fn report_json(r: &cognitive::CognifyReport, filename: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "filename": filename,
        "chunks_added": r.chunks_added,
        "chunks_deduped": r.chunks_deduped,
        "entities_added": r.entities_added,
        "entities_reused": r.entities_reused,
        "edges_added": r.edges_added,
        "edges_strengthened": r.edges_strengthened,
        "llm_skipped": r.llm_skipped,
    })
}

#[derive(Debug, Deserialize)]
pub struct AddTextBody {
    pub text: String,
    #[serde(default)]
    pub source: Option<String>,
    /// Knowledge-base tags (NodeSet scopes). Empty = default memory only.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Also tag into this knowledge space (custom scope id).
    #[serde(default)]
    pub space: Option<String>,
}

pub(crate) async fn cognitive_add(
    State(_s): State<Arc<UiState>>,
    Json(body): Json<AddTextBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    if body.text.trim().is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "text is empty".into()));
    }
    let opts = cognitive::CognifyOptions {
        node_sets: ingest_node_sets(&body.tags, body.space.as_deref()),
        ..Default::default()
    };
    let source = body.source.as_deref().unwrap_or("ui:add");
    let report = sys
        .cognify(&body.text, source, &opts)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report_json(&report, None)))
}

pub(crate) async fn cognitive_upload(
    State(_s): State<Arc<UiState>>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;

    let mut file: Option<(String, String, Vec<u8>)> = None; // (name, content_type, bytes)
    let mut tags: Vec<String> = Vec::new();
    let mut source: Option<String> = None;
    let mut space: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, format!("read multipart: {e}")))?
    {
        match field.name().unwrap_or("") {
            "tags" => {
                if let Ok(t) = field.text().await {
                    tags.extend(
                        t.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    );
                }
            }
            "source" => {
                source = field.text().await.ok().filter(|s| !s.is_empty());
            }
            "space" => {
                space = field.text().await.ok().filter(|s| !s.is_empty());
            }
            _ => {
                // Any other field is treated as the file payload.
                let filename = field.file_name().unwrap_or("upload.txt").to_string();
                let content_type = field.content_type().unwrap_or("").to_string();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AppError(StatusCode::BAD_REQUEST, format!("read file: {e}")))?;
                file = Some((filename, content_type, bytes.to_vec()));
            }
        }
    }

    let (filename, content_type, bytes) =
        file.ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "no file field in upload".into()))?;

    let text = cognitive::extract_text(&filename, &content_type, &bytes)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    if text.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("no extractable text in `{filename}`"),
        ));
    }

    let opts = cognitive::CognifyOptions {
        node_sets: ingest_node_sets(&tags, space.as_deref()),
        ..Default::default()
    };
    let src = source.unwrap_or_else(|| format!("upload:{filename}"));
    let report = sys
        .cognify(&text, &src, &opts)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report_json(&report, Some(&filename))))
}

// =====================================================================
// POST /api/cognitive/recall  { query, mode, limit, hops }
// =====================================================================
//
// Retrieve + LLM synthesis (cognee GRAPH_COMPLETION / "recall" pattern).
// Runs the configured search, numbers the hits `[1]`,`[2]`,… into a context
// block, and asks the cognitive LLM for a grounded answer that cites sources
// as `[n]`. Degrades gracefully: with no LLM configured (or on LLM error) it
// returns the raw matches with `grounded=false` so the UI still shows
// evidence instead of failing.

pub(crate) const RECALL_SYSTEM: &str =
    "You are a precise retrieval assistant. Answer the user's question \
using ONLY the numbered context provided. Cite the sources you use inline as [n]. If the context \
does not contain the answer, say so plainly — do not invent facts. Keep the answer concise and \
in the same language as the question.";

#[derive(Debug, Deserialize)]
pub struct RecallBody {
    pub query: String,
    /// chunks | triplet | graph | spreading | fts | hybrid. Default: graph.
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub hops: Option<u8>,
    /// Restrict to one knowledge space (custom scope id). None = global.
    #[serde(default)]
    pub space: Option<String>,
    /// Recall what was true at this moment instead of now.
    #[serde(default, rename = "asOf")]
    pub as_of: Option<String>,
}

fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

pub(crate) async fn cognitive_recall(
    State(s): State<Arc<UiState>>,
    Json(body): Json<RecallBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let sys = require_system()?;
    if body.query.trim().is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "query is empty".into()));
    }

    let limit = body.limit.unwrap_or(6).clamp(1, 30);
    let mut q = SearchQuery::chunks(body.query.clone(), limit);
    q.query_type = search_type_from_mode(body.mode.as_deref());
    q.hops = body.hops.unwrap_or(2).clamp(1, 6);
    q.decay_per_hop = 0.6;
    q.as_of = as_of_from_body(body.as_of.as_deref())?;
    if let Some(space) = body
        .space
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        q.node_sets = vec![cognitive::NodeSet::space(space)];
    }

    let hits = sys
        .search(&q)
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let sources: Vec<serde_json::Value> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let label = if h.node.name.trim().is_empty() {
                truncate_chars(&h.node.summary, 80)
            } else {
                h.node.name.clone()
            };
            serde_json::json!({
                "index": i + 1,
                "id": h.node.id.to_string(),
                "kind": h.node.kind.as_str(),
                "name": label,
                "summary": truncate_chars(&h.node.summary, 400),
                "score": h.score,
            })
        })
        .collect();

    if hits.is_empty() {
        return Ok(Json(serde_json::json!({
            "answer": "",
            "grounded": false,
            "note": "no matching memories",
            "sources": [],
        })));
    }

    // Build the numbered context fed to the LLM.
    let context = hits
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let text = if h.node.summary.trim().is_empty() {
                h.node.name.clone()
            } else {
                h.node.summary.clone()
            };
            format!("[{}] {}", i + 1, text.trim())
        })
        .collect::<Vec<_>>()
        .join("\n");

    let Some(llm) = cognitive::create_cognitive_llm(s.config.as_ref()) else {
        return Ok(Json(serde_json::json!({
            "answer": "",
            "grounded": false,
            "note": "no cognitive LLM configured — showing raw matches",
            "sources": sources,
        })));
    };

    let user = format!(
        "Context:\n{context}\n\nQuestion: {}\n\nAnswer using only the context above, citing sources as [n].",
        body.query.trim()
    );
    match llm.complete(RECALL_SYSTEM, &user).await {
        Ok(answer) => Ok(Json(serde_json::json!({
            "answer": answer.trim(),
            "grounded": true,
            "sources": sources,
        }))),
        Err(e) => Ok(Json(serde_json::json!({
            "answer": "",
            "grounded": false,
            "note": format!("LLM synthesis failed: {e}"),
            "sources": sources,
        }))),
    }
}

// =====================================================================
// Tests — direct handler invocation (no axum boot)
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::cognitive::data_point::NodeKind;

    #[test]
    fn node_view_roundtrip() {
        let mut n = DataPoint::entity("Ada", 100);
        n.summary = "pioneer".into();
        let v: NodeView = n.into();
        assert_eq!(v.kind, "entity");
        assert_eq!(v.name, "Ada");
        assert_eq!(v.summary, "pioneer");
    }

    #[test]
    fn mode_string_maps_to_search_type() {
        assert_eq!(search_type_from_mode(Some("fts")), SearchType::Fts);
        assert_eq!(search_type_from_mode(Some("hybrid")), SearchType::Hybrid);
        assert_eq!(search_type_from_mode(Some("chunks")), SearchType::Chunks);
        // unknown / missing → graph completion
        assert_eq!(search_type_from_mode(None), SearchType::GraphCompletion);
        assert_eq!(
            search_type_from_mode(Some("???")),
            SearchType::GraphCompletion
        );
    }

    #[test]
    fn ingest_node_sets_always_includes_default_and_dedupes() {
        let sets = ingest_node_sets(&["kb-a".into(), " ".into(), "default_memory".into()], None);
        // default + kb-a (blank and explicit default are dropped)
        assert_eq!(sets.len(), 2);
        assert!(sets.iter().any(|s| s.tag == "default_memory"));
        assert!(sets.iter().any(|s| s.tag == "kb-a"));
        assert!(sets
            .iter()
            .all(|s| s.scope_kind == crate::memory::cognitive::ScopeKind::Global));
    }

    #[test]
    fn ingest_node_sets_adds_space_scope() {
        let sets = ingest_node_sets(&[], Some("ai-office:nghien-cuu"));
        assert_eq!(sets.len(), 2);
        assert!(sets.iter().any(|s| {
            s.scope_kind == crate::memory::cognitive::ScopeKind::Custom
                && s.scope_id == "ai-office:nghien-cuu"
        }));
        // blank space is ignored
        assert_eq!(ingest_node_sets(&[], Some("  ")).len(), 1);
    }

    #[test]
    fn truncate_chars_is_unicode_safe() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
        // multibyte must not panic / split a char
        assert_eq!(truncate_chars("càphê", 2), "cà…");
    }

    #[test]
    fn edge_view_carries_dynamics() {
        let mut e = RelationshipEdge::new(Uuid::new_v4(), Uuid::new_v4(), "knows", 10);
        e.strength = 0.42;
        e.activation_count = 7;
        let v: EdgeView = e.into();
        assert_eq!(v.predicate, "knows");
        assert!((v.strength - 0.42).abs() < 1e-5);
        assert_eq!(v.activation_count, 7);
    }

    #[test]
    fn parse_uuid_rejects_garbage() {
        assert!(parse_uuid("nope").is_err());
        assert!(parse_uuid(&Uuid::new_v4().to_string()).is_ok());
    }

    #[test]
    fn require_system_returns_503_when_dormant() {
        // Default test environment hasn't booted the daemon, so the
        // singleton is empty.
        match require_system() {
            Err(AppError(code, msg)) => {
                assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
                assert!(msg.contains("dormant"), "{msg}");
            }
            Ok(_) => {
                // If another test bootstrapped the singleton, that's fine —
                // we just can't run this assertion in that case.
            }
        }
        // touch NodeKind so the import isn't unused if future tests are pruned
        let _ = NodeKind::Entity;
    }
}

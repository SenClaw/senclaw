//! GraphStore trait + SQLite implementation.
//!
//! Backend default is SQLite (no extra deps). The trait keeps the door open
//! for optional Kuzu (feature `cognitive-kuzu`) later — see plan P5.

use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use crate::db::Db;

use super::data_point::{DataPoint, NodeKind};
use super::ltp::LtpStatus;
use super::node_set::NodeSet;
use super::predicate_meta::{normalize as normalize_predicate, Cardinality};
use super::tiers::EdgeTier;
use super::triplet::RelationshipEdge;

/// Storage backend for the cognitive graph. Each method is sync because the
/// senclaw `Db` holds a `Mutex<Connection>` — callers wrap in `spawn_blocking`
/// when serving async contexts.
pub trait GraphStore: Send + Sync {
    fn upsert_node(&self, node: &DataPoint) -> Result<()>;
    /// Update the extraction-state machine on a chunk node. Called by
    /// the cognify pipeline after each LLM attempt. No-op on missing id.
    fn set_extraction_state(
        &self,
        id: Uuid,
        state: crate::memory::cognitive::ExtractionState,
        at: i64,
    ) -> Result<()>;
    fn get_node(&self, id: Uuid) -> Result<Option<DataPoint>>;
    fn find_node_by_content_hash(&self, hash: &str) -> Result<Option<DataPoint>>;
    fn find_entity_by_name(&self, name: &str) -> Result<Option<DataPoint>>;
    fn delete_node(&self, id: Uuid) -> Result<()>;

    /// BM25 full-text search over the `cog_nodes_fts` index (node name +
    /// summary). `fts_match` is a ready-built FTS5 MATCH expression — the
    /// caller (retriever) owns tokenisation so storage stays NLP-free.
    /// `kind` optionally restricts results to one [`NodeKind`] string.
    /// Returns `(node, score)` with score normalised to `[0, 1]` (1 = best
    /// BM25 rank), mirroring `memory::fts_search`. Needs no embedder — this
    /// is the zero-cost retrieval path.
    fn fts_search_nodes(
        &self,
        fts_match: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(DataPoint, f32)>>;

    fn upsert_edge(&self, edge: &RelationshipEdge) -> Result<()>;
    fn delete_edge(&self, src: Uuid, dst: Uuid, predicate: &str) -> Result<()>;

    /// Every edge incident to `node`, superseded ones included. This is the
    /// *write-path* view: cognify needs to see an invalidated fact in order
    /// to re-assert it. Retrieval must use [`Self::neighbors_at`] instead.
    fn neighbors(&self, node: Uuid, max: usize) -> Result<Vec<RelationshipEdge>>;

    /// Edges incident to `node` that hold at the given world time.
    ///
    /// * `as_of = None` — what the graph believes **now**: `valid_to IS NULL`.
    ///   This is the default for every retrieval, which is what stops a
    ///   superseded price from being recalled as the current one.
    /// * `as_of = Some(t)` — time travel: `valid_from <= t < valid_to`.
    ///
    /// Archived (dormant) edges are still returned in both modes — dormancy
    /// down-ranks a fact, it does not make it untrue.
    fn neighbors_at(
        &self,
        node: Uuid,
        max: usize,
        as_of: Option<i64>,
    ) -> Result<Vec<RelationshipEdge>>;

    /// Facts currently held for `(src, predicate)` — the lookup that decides
    /// what a newly extracted fact supersedes.
    fn current_edges_for(&self, src: Uuid, predicate: &str) -> Result<Vec<RelationshipEdge>>;

    /// Every version of `(src, predicate)` ordered oldest-first, superseded
    /// ones included: the timeline behind `cog_history`.
    fn edge_history(&self, src: Uuid, predicate: Option<&str>, limit: usize)
        -> Result<Vec<RelationshipEdge>>;

    /// Cardinality for a predicate; `Multi` when the table has no row (see
    /// [`super::predicate_meta`] for why unknown must never supersede).
    fn predicate_cardinality(&self, predicate: &str) -> Result<Cardinality>;
    fn set_predicate_cardinality(
        &self,
        predicate: &str,
        cardinality: Cardinality,
        source: &str,
    ) -> Result<()>;
    fn list_predicate_meta(&self) -> Result<Vec<PredicateMetaRow>>;

    /// Pull a batch of **active** (non-archived) edges
    /// ordered by `last_activated ASC` (stalest first). `offset` lets the
    /// decay tick page through the table in chunks without holding
    /// everything in memory. Archived edges are frozen — scanning them
    /// every tick would be pure waste, so they're filtered at the SQL level.
    fn scan_edges(&self, limit: usize, offset: usize) -> Result<Vec<RelationshipEdge>>;
    fn count_edges(&self) -> Result<usize>;
    /// Write the result of a decay sweep into `cog_decay_log`.
    fn record_decay_run(
        &self,
        run_at: i64,
        edges_scanned: usize,
        edges_pruned: usize,
        edges_promoted: usize,
        duration_ms: i64,
    ) -> Result<()>;

    /// Reset `extraction_state` back to `Pending` for chunks that were
    /// extracted (`Done`) but have since lost every outgoing edge — i.e.
    /// their MENTIONS/semantic edges decayed away, so the facts are gone
    /// while the dedupe gate still says "already processed". A backfill
    /// pass (POST /api/cognitive/re-extract-pending) then picks them up
    /// like never-extracted chunks. Returns the number of chunks reset.
    fn reset_orphan_done_chunks(&self) -> Result<usize>;

    /// Every NodeSet the given node is tagged into — reverse of
    /// [`Self::tag_node`]. Lets re-extraction tag recovered entities into
    /// the chunk's original knowledge spaces instead of leaving them
    /// unscoped.
    fn sets_of_node(&self, node: Uuid) -> Result<Vec<NodeSet>>;

    fn tag_node(&self, node: Uuid, set: &NodeSet) -> Result<()>;
    fn nodes_in_set(&self, set: &NodeSet, limit: usize) -> Result<Vec<DataPoint>>;

    /// Edges whose BOTH endpoints are tagged into the given NodeSet —
    /// the edge set of one knowledge space's induced subgraph. Powers the
    /// space-scoped Knowledge graph view (`/api/cognitive/full-graph?space=`).
    /// Ordered by `last_activated DESC` so a truncated result keeps the
    /// liveliest edges.
    fn edges_within_set(&self, set: &NodeSet, limit: usize) -> Result<Vec<RelationshipEdge>>;

    /// All node ids tagged into ANY of the given sets. Used to restrict
    /// search results to a knowledge space (any-of semantics).
    fn node_ids_in_sets(&self, sets: &[NodeSet]) -> Result<std::collections::HashSet<Uuid>>;

    /// Registry of every node set with its member count — the "knowledge
    /// spaces" listing for the UI/API.
    fn list_node_sets(&self) -> Result<Vec<NodeSetInfo>>;

    /// Paginated node listing for the Web UI. `kind=None` returns all kinds.
    fn list_nodes(&self, kind: Option<&str>, limit: usize, offset: usize)
        -> Result<Vec<DataPoint>>;
    fn count_nodes(&self, kind: Option<&str>) -> Result<usize>;
    fn recent_decay_runs(&self, limit: usize) -> Result<Vec<DecayLogRow>>;

    /// Return the top-`limit` nodes ordered by incident-edge count
    /// descending. Used by the Graph Explorer to surface "interesting"
    /// nodes — high-degree entities are usually the natural seeds for
    /// browsing a knowledge graph.
    fn top_nodes_by_degree(&self, limit: usize) -> Result<Vec<NodeWithDegree>>;

    fn full_graph(
        &self,
        node_limit: usize,
        edge_limit: usize,
        include_chunks: bool,
    ) -> Result<(Vec<DataPoint>, Vec<RelationshipEdge>)>;

    /// Return edges that originate from a node inside the given NodeSet.
    /// Used by the persona-consolidate path to find "what the agent has
    /// learned about itself" and pour it back into SOUL.md.
    ///
    /// `min_strength` filters out weak/decaying edges; `require_ltp` keeps
    /// only edges that have hit any LTP state (Burst/Weekly/Full) so the
    /// resulting facts are ones the graph considers durable.
    fn edges_from_set(
        &self,
        set: &NodeSet,
        min_strength: f32,
        require_ltp: bool,
        limit: usize,
    ) -> Result<Vec<(RelationshipEdge, DataPoint, DataPoint)>>;

    /// Bulk-delete junk nodes. Used by the Data memory cleanup button and
    /// the maintenance sweep. Six signals identify junk, applied in order
    /// (earlier deletions cascade edges and can orphan nodes the later
    /// passes then catch):
    ///   * `chunk` nodes whose `summary` contains any of the well-known
    ///     envelope markers (`<messages>`, `<message ` etc.). These get
    ///     past the runtime sanitizer because they were ingested BEFORE
    ///     we added it.
    ///   * `chunk` nodes that today's [`sanitize_for_cognify`] would reject
    ///     (markup-heavy HTML/JSON dumps, too short after stripping) —
    ///     retroactive parity with the ingest guard for legacy rows.
    ///   * `entity` nodes whose name carries no alphanumeric character at
    ///     all (pure punctuation/symbols) — extraction artifacts. Pure-digit
    ///     names are KEPT: "2026" can be a real date object in a triplet.
    ///   * Any `entity` whose UUID appears nowhere in `cog_edges`. These
    ///     are leftovers from prior `forget` operations that removed the
    ///     incident edges but left the node.
    ///   * `entity` nodes whose every incident edge points at an
    ///     `entity_type` node (only `is_a`, no chunk MENTIONS it, no
    ///     semantic relation) — ungrounded extraction artifacts.
    ///   * `entity_type` nodes left with zero incident edges after the
    ///     passes above.
    /// Cascade deletes any incident edges too (FK ON DELETE CASCADE
    /// from the schema).
    fn cleanup_junk(&self) -> Result<CleanupReport>;

    /// Top entity names (highest `mention_count` first) — feeds the cognify
    /// `known_entities` prompt hint so chunk N reuses the names chunk N-1
    /// created instead of minting aliases ("Hà Nội" vs "HN"). `set` scopes
    /// the lookup to one NodeSet (typically the chat group); `None` falls
    /// back to the global top entities.
    fn top_entity_names(&self, set: Option<&NodeSet>, limit: usize) -> Result<Vec<String>>;

    /// Alias merge — the embedding-based cousin of
    /// [`merge_duplicate_entities`] (the "P4" step deferred at entity
    /// resolution). Merges entity pairs whose stored `cog_nodes.embedding`
    /// vectors reach `min_cosine` similarity AND whose `type_name` matches
    /// (case-insensitive). Candidates are capped to the `max_candidates`
    /// highest-mention entities so the pairwise scan stays bounded
    /// (O(max_candidates²) dot products, fine for a daily sweep).
    /// Canonical = higher mention_count, tie-broken by oldest. Zero LLM /
    /// embedding calls — vectors were paid for at ingest.
    fn merge_alias_entities(
        &self,
        min_cosine: f32,
        max_candidates: usize,
    ) -> Result<AliasMergeReport>;

    /// Merge duplicate `entity` nodes that share the same case-insensitive
    /// `name`. For each duplicate group:
    ///   * Pick a canonical (highest `mention_count`, tie-broken by oldest
    ///     `created_at`).
    ///   * Redirect every incident edge from the duplicates to the canonical
    ///     using `INSERT OR IGNORE` so PK collisions silently coalesce.
    ///   * Bump the canonical's `mention_count` by the sum of the merged
    ///     mention counts and refresh `last_seen_at`.
    ///   * Delete the duplicate nodes.
    /// Returns counts for the UI summary.
    fn merge_duplicate_entities(&self) -> Result<MergeReport>;

    /// Associative inference — the "suy luận liên kết thông tin" step.
    ///
    /// Cognee enriches its graph by linking entities that the LLM connected
    /// explicitly. We add a cheaper, LLM-free layer: entities that are
    /// *co-mentioned* by the same chunk (both are `dst` of a `MENTIONS`
    /// edge from the same chunk `src`) clearly relate to one another even
    /// when no extracted triplet links them directly. For every such pair
    /// with co-occurrence ≥ `min_cooccurrence` and **no** existing edge in
    /// either direction, we materialise an `ASSOCIATED_WITH` edge whose
    /// strength scales with how often the two co-occur.
    ///
    /// These inferred edges participate in spreading-activation retrieval
    /// and tier decay exactly like extracted ones, so a weak guess that is
    /// never reinforced simply decays away. `max_per_run` caps the strongest
    /// candidates so a dense graph can't explode in one pass.
    fn infer_associative_edges(
        &self,
        min_cooccurrence: usize,
        max_per_run: usize,
    ) -> Result<InferenceReport>;
}

/// Result of a [`GraphStore::cleanup_junk`] call. Returned to the HTTP
/// caller so the UI can summarise "removed 12 envelope-chunks, 3 orphans".
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct CleanupReport {
    pub envelope_chunks_removed: usize,
    /// Chunks the current `sanitize_for_cognify` would reject (markup-heavy
    /// dumps) — legacy rows ingested before the guard existed.
    pub markup_chunks_removed: usize,
    /// Entities whose name has no alphanumeric character (pure symbols).
    pub junk_entities_removed: usize,
    pub orphan_entities_removed: usize,
    /// Entities grounded by nothing but `is_a → entity_type` edges.
    pub typeonly_entities_removed: usize,
    /// `entity_type` nodes orphaned by the passes above.
    pub orphan_type_nodes_removed: usize,
}

impl CleanupReport {
    /// Total nodes removed across every category — UI summary line.
    pub fn total_removed(&self) -> usize {
        self.envelope_chunks_removed
            + self.markup_chunks_removed
            + self.junk_entities_removed
            + self.orphan_entities_removed
            + self.typeonly_entities_removed
            + self.orphan_type_nodes_removed
    }
}

/// Result of a [`GraphStore::merge_duplicate_entities`] call.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct MergeReport {
    /// Number of duplicate-name groups that had at least one merge applied.
    pub groups_merged: usize,
    /// Total entity nodes deleted (= sum of (group_size - 1) across groups).
    pub entities_merged: usize,
    /// Edges that survived redirection (re-pointed to canonical).
    pub edges_redirected: usize,
}

/// Result of a [`GraphStore::merge_alias_entities`] call.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct AliasMergeReport {
    /// Same-type candidate pairs whose cosine similarity was computed.
    pub pairs_examined: usize,
    /// Duplicate entity nodes deleted (merged into a canonical).
    pub entities_merged: usize,
    /// Edges that survived redirection to the canonical.
    pub edges_redirected: usize,
}

/// Result of a [`GraphStore::infer_associative_edges`] call.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct InferenceReport {
    /// Co-occurring entity pairs examined as candidates.
    pub candidates_examined: usize,
    /// New `ASSOCIATED_WITH` edges materialised this pass.
    pub associations_created: usize,
}

/// `DataPoint` paired with its incident-edge count. Returned by
/// [`GraphStore::top_nodes_by_degree`] for the Graph Explorer UI.
#[derive(Debug, Clone)]
pub struct NodeWithDegree {
    pub node: DataPoint,
    pub degree: usize,
}

/// Row shape returned by [`GraphStore::list_node_sets`] — one knowledge
/// space/scope with its member count.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeSetInfo {
    #[serde(rename = "scopeKind")]
    pub scope_kind: String,
    #[serde(rename = "scopeId")]
    pub scope_id: String,
    pub tag: String,
    pub nodes: usize,
}

/// Row shape returned by [`GraphStore::list_predicate_meta`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct PredicateMetaRow {
    pub predicate: String,
    pub cardinality: String,
    pub source: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
}

/// Row shape returned by [`GraphStore::recent_decay_runs`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct DecayLogRow {
    pub run_at: i64,
    pub edges_scanned: usize,
    pub edges_pruned: usize,
    pub edges_promoted: usize,
    pub duration_ms: i64,
}

pub struct SqliteGraphStore {
    db: Arc<Db>,
}

impl SqliteGraphStore {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }
}

// ===== helpers =====

fn uuid_bytes(u: Uuid) -> [u8; 16] {
    *u.as_bytes()
}

/// Redirect every incident edge from `dup_id` onto `canonical_id`
/// (INSERT OR IGNORE silently drops PK collisions where the canonical
/// already holds the same edge), consolidate the duplicate's information
/// onto the canonical (space tags, summary), then delete the duplicate
/// node. Returns the number of redirected edges that survived. Shared by
/// the name-based and embedding-based entity merges — mention-count rollup
/// stays with the callers, which batch it differently.
///
/// A merge must never LOSE information: without the tag copy the
/// duplicate's `cog_node_tags` rows die with it (FK cascade) and every
/// knowledge space that only knew the duplicate silently drops the entity.
fn redirect_edges_and_delete(
    conn: &rusqlite::Connection,
    canonical_id: &[u8],
    dup_id: &[u8],
) -> rusqlite::Result<usize> {
    // Union the duplicate's space memberships onto the canonical BEFORE the
    // delete cascades them away.
    conn.execute(
        "INSERT OR IGNORE INTO cog_node_tags (node_id, node_set_id)
         SELECT ?1, node_set_id FROM cog_node_tags WHERE node_id = ?2",
        rusqlite::params![canonical_id, dup_id],
    )?;
    // Adopt the duplicate's summary when the canonical has none — merged
    // knowledge is consolidated, not discarded.
    conn.execute(
        "UPDATE cog_nodes
         SET summary = (SELECT summary FROM cog_nodes WHERE id = ?2)
         WHERE id = ?1
           AND TRIM(summary) = ''
           AND (SELECT TRIM(summary) FROM cog_nodes WHERE id = ?2) <> ''",
        rusqlite::params![canonical_id, dup_id],
    )?;
    const EDGE_COLS: &str = "predicate, props_json,
         valid_from, valid_to, invalidated_by, archived_at,
         strength, tier, activation_count, last_activated,
         ltp_status, ltp_detected_at,
         entity_confidence, endpoint_selectivity, forman_curvature,
         activation_timestamps,
         source_episode_id, context, created_at";
    // Outgoing edges.
    let inserted_src = conn.execute(
        &format!(
            "INSERT OR IGNORE INTO cog_edges (src, dst, {EDGE_COLS})
             SELECT ?1, dst, {EDGE_COLS} FROM cog_edges WHERE src = ?2"
        ),
        rusqlite::params![canonical_id, dup_id],
    )?;
    conn.execute(
        "DELETE FROM cog_edges WHERE src = ?1",
        rusqlite::params![dup_id],
    )?;
    // Incoming edges.
    let inserted_dst = conn.execute(
        &format!(
            "INSERT OR IGNORE INTO cog_edges (src, dst, {EDGE_COLS})
             SELECT src, ?1, {EDGE_COLS} FROM cog_edges WHERE dst = ?2"
        ),
        rusqlite::params![canonical_id, dup_id],
    )?;
    conn.execute(
        "DELETE FROM cog_edges WHERE dst = ?1",
        rusqlite::params![dup_id],
    )?;
    // Delete the duplicate node (FK cascade no-ops — edges already gone).
    conn.execute(
        "DELETE FROM cog_nodes WHERE id = ?1",
        rusqlite::params![dup_id],
    )?;
    Ok(inserted_src + inserted_dst)
}

/// Append `alias` to the `aka` string array inside a node's `props_json`
/// (deduped, case-insensitive). Keeps absorbed surface names queryable
/// after an alias merge instead of silently dropping them.
fn record_alias(
    conn: &rusqlite::Connection,
    node_id: &[u8],
    alias: &str,
) -> rusqlite::Result<()> {
    let props_str: String = conn.query_row(
        "SELECT props_json FROM cog_nodes WHERE id = ?1",
        rusqlite::params![node_id],
        |r| r.get(0),
    )?;
    let mut props: Value =
        serde_json::from_str(&props_str).unwrap_or(Value::Object(Default::default()));
    if !props.is_object() {
        props = Value::Object(Default::default());
    }
    let obj = props.as_object_mut().expect("coerced to object above");
    let aka = obj
        .entry("aka")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !aka.is_array() {
        *aka = Value::Array(Vec::new());
    }
    let list = aka.as_array_mut().expect("coerced to array above");
    let exists = list.iter().any(|v| {
        v.as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(alias))
    });
    if !exists {
        list.push(Value::String(alias.to_string()));
        let serialized = serde_json::to_string(&props).unwrap_or_else(|_| "{}".into());
        conn.execute(
            "UPDATE cog_nodes SET props_json = ?2 WHERE id = ?1",
            rusqlite::params![node_id, serialized],
        )?;
    }
    Ok(())
}

fn bytes_uuid(b: Vec<u8>) -> Result<Uuid> {
    let arr: [u8; 16] = b
        .as_slice()
        .try_into()
        .context("uuid blob must be 16 bytes")?;
    Ok(Uuid::from_bytes(arr))
}

/// Decode the 32-char uppercase hex string `cog_nodes_fts.node_id` (produced
/// by SQLite `hex(id)` in the FTS triggers) back into a [`Uuid`]. Returns
/// `None` on any malformed value so a stray FTS row can't poison a search.
fn hex_to_uuid(hex: &str) -> Option<Uuid> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(Uuid::from_bytes(bytes))
}

fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<DataPoint> {
    let id_bytes: Vec<u8> = row.get("id")?;
    let id = bytes_uuid(id_bytes).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, e.into())
    })?;
    let kind: String = row.get("kind")?;
    let props_str: String = row.get("props_json")?;
    let props: Value = serde_json::from_str(&props_str).unwrap_or(Value::Null);
    Ok(DataPoint {
        id,
        kind: NodeKind::from_str(&kind),
        type_name: row.get("type_name")?,
        name: row.get("name")?,
        summary: row.get("summary")?,
        content_hash: row.get::<_, Option<String>>("content_hash")?,
        props,
        salience: row.get::<_, f64>("salience")? as f32,
        mention_count: row.get::<_, i64>("mention_count")? as u32,
        is_proper_noun: row.get::<_, i64>("is_proper_noun")? != 0,
        selectivity: row.get::<_, Option<f64>>("selectivity")?.map(|v| v as f32),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        last_seen_at: row.get("last_seen_at")?,
        extraction_state: super::data_point::ExtractionState::from_i64(
            row.get::<_, i64>("extraction_state").unwrap_or(0),
        ),
        extracted_at: row.get::<_, Option<i64>>("extracted_at").unwrap_or(None),
    })
}

fn row_to_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<RelationshipEdge> {
    let to_uuid = |col: &str| -> rusqlite::Result<Uuid> {
        let b: Vec<u8> = row.get(col)?;
        bytes_uuid(b).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, e.into())
        })
    };
    let src = to_uuid("src")?;
    let dst = to_uuid("dst")?;
    let props_str: String = row.get("props_json")?;
    let props: Value = serde_json::from_str(&props_str).unwrap_or(Value::Null);
    let act_str: String = row.get("activation_timestamps")?;
    let activation_timestamps: Vec<i64> = serde_json::from_str(&act_str).unwrap_or_default();
    let src_ep: Option<Vec<u8>> = row.get("source_episode_id")?;
    let source_episode_id = src_ep.map(bytes_uuid).transpose().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, e.into())
    })?;
    let inv_by: Option<Vec<u8>> = row.get("invalidated_by")?;
    let invalidated_by = inv_by.map(bytes_uuid).transpose().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, e.into())
    })?;
    Ok(RelationshipEdge {
        src,
        dst,
        predicate: row.get("predicate")?,
        props,
        valid_from: row.get("valid_from")?,
        valid_to: row.get("valid_to")?,
        invalidated_by,
        archived_at: row.get("archived_at")?,
        strength: row.get::<_, f64>("strength")? as f32,
        tier: EdgeTier::from_u8(row.get::<_, i64>("tier")? as u8),
        activation_count: row.get::<_, i64>("activation_count")? as u32,
        last_activated: row.get("last_activated")?,
        ltp_status: LtpStatus::from_u8(row.get::<_, i64>("ltp_status")? as u8),
        ltp_detected_at: row.get("ltp_detected_at")?,
        entity_confidence: row
            .get::<_, Option<f64>>("entity_confidence")?
            .map(|v| v as f32),
        endpoint_selectivity: row
            .get::<_, Option<f64>>("endpoint_selectivity")?
            .map(|v| v as f32),
        forman_curvature: row
            .get::<_, Option<f64>>("forman_curvature")?
            .map(|v| v as f32),
        activation_timestamps,
        source_episode_id,
        context: row.get("context")?,
        created_at: row.get("created_at")?,
    })
}

// ===== impl =====

impl GraphStore for SqliteGraphStore {
    fn upsert_node(&self, node: &DataPoint) -> Result<()> {
        let id = uuid_bytes(node.id).to_vec();
        let props_json = serde_json::to_string(&node.props).unwrap_or_else(|_| "{}".into());
        self.db.with_cog_conn(|conn| {
            conn.execute(
                r#"INSERT INTO cog_nodes
                   (id, kind, type_name, name, summary, content_hash, props_json,
                    salience, mention_count, is_proper_noun, selectivity,
                    created_at, updated_at, last_seen_at,
                    extraction_state, extracted_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                   ON CONFLICT(id) DO UPDATE SET
                     summary          = excluded.summary,
                     props_json       = excluded.props_json,
                     salience         = excluded.salience,
                     mention_count    = excluded.mention_count,
                     selectivity      = excluded.selectivity,
                     updated_at       = excluded.updated_at,
                     last_seen_at     = excluded.last_seen_at,
                     -- Persist state advances on conflict, but never demote:
                     -- a `done` row stays `done` even if the caller passes
                     -- `pending` (e.g. building a DataPoint from a partial
                     -- in-memory copy without consulting the DB first).
                     extraction_state = MAX(extraction_state, excluded.extraction_state),
                     extracted_at     = COALESCE(excluded.extracted_at, extracted_at)"#,
                params![
                    id,
                    node.kind.as_str(),
                    node.type_name,
                    node.name,
                    node.summary,
                    node.content_hash,
                    props_json,
                    node.salience as f64,
                    node.mention_count as i64,
                    node.is_proper_noun as i64,
                    node.selectivity.map(|v| v as f64),
                    node.created_at,
                    node.updated_at,
                    node.last_seen_at,
                    node.extraction_state as i64,
                    node.extracted_at,
                ],
            )?;
            Ok(())
        })
    }

    fn set_extraction_state(
        &self,
        id: Uuid,
        state: crate::memory::cognitive::ExtractionState,
        at: i64,
    ) -> Result<()> {
        let id_blob = uuid_bytes(id).to_vec();
        self.db.with_cog_conn(|conn| {
            conn.execute(
                "UPDATE cog_nodes
                 SET extraction_state = ?1,
                     extracted_at     = ?2,
                     updated_at       = ?2
                 WHERE id = ?3",
                params![state as i64, at, id_blob],
            )?;
            Ok(())
        })
    }

    fn get_node(&self, id: Uuid) -> Result<Option<DataPoint>> {
        let id_blob = uuid_bytes(id).to_vec();
        self.db.with_cog_conn(|conn| {
            let row = conn
                .query_row(
                    "SELECT * FROM cog_nodes WHERE id = ?1",
                    params![id_blob],
                    row_to_node,
                )
                .optional()?;
            Ok(row)
        })
    }

    fn find_node_by_content_hash(&self, hash: &str) -> Result<Option<DataPoint>> {
        self.db.with_cog_conn(|conn| {
            let row = conn
                .query_row(
                    "SELECT * FROM cog_nodes WHERE content_hash = ?1 LIMIT 1",
                    params![hash],
                    row_to_node,
                )
                .optional()?;
            Ok(row)
        })
    }

    fn find_entity_by_name(&self, name: &str) -> Result<Option<DataPoint>> {
        // Case-insensitive + trimmed match, using the SAME `LOWER(TRIM(name))`
        // identity that `merge_duplicate_entities` collapses on. Resolving on
        // this key at ingest time means edges attach to one canonical entity
        // immediately instead of accreting "Ada"/"ada"/" Ada " variants that
        // the maintenance sweep later has to merge. (SQLite LOWER is ASCII-
        // only — matching the existing merge behaviour; full-Unicode folding
        // would need a stored normalised column, a future enhancement.)
        self.db.with_cog_conn(|conn| {
            let row = conn
                .query_row(
                    "SELECT * FROM cog_nodes
                     WHERE kind = 'entity' AND LOWER(TRIM(name)) = LOWER(TRIM(?1))
                     ORDER BY mention_count DESC, created_at ASC
                     LIMIT 1",
                    params![name],
                    row_to_node,
                )
                .optional()?;
            Ok(row)
        })
    }

    fn delete_node(&self, id: Uuid) -> Result<()> {
        let id_blob = uuid_bytes(id).to_vec();
        self.db.with_cog_conn(|conn| {
            conn.execute("DELETE FROM cog_nodes WHERE id = ?1", params![id_blob])?;
            Ok(())
        })
    }

    fn fts_search_nodes(
        &self,
        fts_match: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(DataPoint, f32)>> {
        if fts_match.trim().is_empty() || limit == 0 {
            return Ok(vec![]);
        }
        // Over-fetch when kind-filtering so we still reach `limit` after the
        // Rust-side filter (mirrors `vector_seeds`). The join back to
        // cog_nodes is done per-id rather than in SQL so the FTS query stays
        // index-only and we avoid an `unhex()` version dependency.
        let fetch = if kind.is_some() {
            (limit * 4).max(8)
        } else {
            limit
        };
        self.db.with_cog_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT node_id, bm25(cog_nodes_fts) AS rank
                 FROM cog_nodes_fts
                 WHERE text MATCH ?1
                 ORDER BY rank
                 LIMIT ?2",
            )?;
            let raw: Vec<(String, f64)> = stmt
                .query_map(params![fts_match, fetch as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            if raw.is_empty() {
                return Ok(vec![]);
            }
            // BM25: smaller rank = better. Normalise to [0,1], 1 = best.
            let ranks: Vec<f64> = raw.iter().map(|(_, r)| *r).collect();
            let min = ranks.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = ranks.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let range = max - min;

            let mut out: Vec<(DataPoint, f32)> = Vec::with_capacity(raw.len().min(limit));
            for (hex_id, rank) in raw {
                let Some(id) = hex_to_uuid(&hex_id) else {
                    continue;
                };
                let id_blob = uuid_bytes(id).to_vec();
                let node: Option<DataPoint> = conn
                    .query_row(
                        "SELECT * FROM cog_nodes WHERE id = ?1",
                        params![id_blob],
                        row_to_node,
                    )
                    .optional()?;
                let Some(node) = node else { continue };
                if let Some(k) = kind {
                    if node.kind.as_str() != k {
                        continue;
                    }
                }
                let score = if range == 0.0 {
                    1.0
                } else {
                    ((max - rank) / range) as f32
                };
                out.push((node, score));
                if out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        })
    }

    fn upsert_edge(&self, edge: &RelationshipEdge) -> Result<()> {
        let src = uuid_bytes(edge.src).to_vec();
        let dst = uuid_bytes(edge.dst).to_vec();
        let props = serde_json::to_string(&edge.props).unwrap_or_else(|_| "{}".into());
        let acts =
            serde_json::to_string(&edge.activation_timestamps).unwrap_or_else(|_| "[]".into());
        let ep_id = edge.source_episode_id.map(|u| uuid_bytes(u).to_vec());
        self.db.with_cog_conn(|conn| {
            conn.execute(
                r#"INSERT INTO cog_edges
                   (src, dst, predicate, props_json, valid_from, valid_to,
                    invalidated_by, archived_at,
                    strength, tier, activation_count, last_activated,
                    ltp_status, ltp_detected_at, entity_confidence,
                    endpoint_selectivity, forman_curvature, activation_timestamps,
                    source_episode_id, context, created_at)
                   VALUES (?1,?2,?3,?4,?5,?6,?20,?21,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
                   ON CONFLICT(src, dst, predicate) DO UPDATE SET
                     props_json            = excluded.props_json,
                     valid_from            = excluded.valid_from,
                     valid_to              = excluded.valid_to,
                     invalidated_by        = excluded.invalidated_by,
                     archived_at           = excluded.archived_at,
                     strength              = excluded.strength,
                     tier                  = excluded.tier,
                     activation_count      = excluded.activation_count,
                     last_activated        = excluded.last_activated,
                     ltp_status            = excluded.ltp_status,
                     ltp_detected_at       = excluded.ltp_detected_at,
                     entity_confidence     = excluded.entity_confidence,
                     endpoint_selectivity  = excluded.endpoint_selectivity,
                     forman_curvature      = excluded.forman_curvature,
                     activation_timestamps = excluded.activation_timestamps,
                     context               = excluded.context"#,
                params![
                    src,
                    dst,
                    edge.predicate,
                    props,
                    edge.valid_from,
                    edge.valid_to,
                    edge.strength as f64,
                    edge.tier as u8 as i64,
                    edge.activation_count as i64,
                    edge.last_activated,
                    edge.ltp_status as u8 as i64,
                    edge.ltp_detected_at,
                    edge.entity_confidence.map(|v| v as f64),
                    edge.endpoint_selectivity.map(|v| v as f64),
                    edge.forman_curvature.map(|v| v as f64),
                    acts,
                    ep_id,
                    edge.context,
                    edge.created_at,
                    edge.invalidated_by.map(|u| uuid_bytes(u).to_vec()),
                    edge.archived_at,
                ],
            )?;
            Ok(())
        })
    }

    fn delete_edge(&self, src: Uuid, dst: Uuid, predicate: &str) -> Result<()> {
        let src = uuid_bytes(src).to_vec();
        let dst = uuid_bytes(dst).to_vec();
        self.db.with_cog_conn(|conn| {
            conn.execute(
                "DELETE FROM cog_edges WHERE src = ?1 AND dst = ?2 AND predicate = ?3",
                params![src, dst, predicate],
            )?;
            Ok(())
        })
    }

    fn neighbors(&self, node: Uuid, max: usize) -> Result<Vec<RelationshipEdge>> {
        let id = uuid_bytes(node).to_vec();
        self.db.with_cog_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT * FROM cog_edges
                 WHERE src = ?1 OR dst = ?1
                 ORDER BY last_activated DESC
                 LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![id, max as i64], row_to_edge)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn neighbors_at(
        &self,
        node: Uuid,
        max: usize,
        as_of: Option<i64>,
    ) -> Result<Vec<RelationshipEdge>> {
        let id = uuid_bytes(node).to_vec();
        self.db.with_cog_conn(|conn| {
            // Two shapes rather than one clever SQL: "current" is served by
            // the partial index on (src, predicate) WHERE valid_to IS NULL,
            // which a `?2 IS NULL OR …` predicate would not be able to use.
            let rows = match as_of {
                None => {
                    let mut stmt = conn.prepare(
                        "SELECT * FROM cog_edges
                         WHERE (src = ?1 OR dst = ?1) AND valid_to IS NULL
                         ORDER BY last_activated DESC
                         LIMIT ?2",
                    )?;
                    let out = stmt
                        .query_map(params![id, max as i64], row_to_edge)?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    out
                }
                Some(t) => {
                    let mut stmt = conn.prepare(
                        "SELECT * FROM cog_edges
                         WHERE (src = ?1 OR dst = ?1)
                           AND valid_from <= ?2
                           AND (valid_to IS NULL OR valid_to > ?2)
                         ORDER BY last_activated DESC
                         LIMIT ?3",
                    )?;
                    let out = stmt
                        .query_map(params![id, t, max as i64], row_to_edge)?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    out
                }
            };
            Ok(rows)
        })
    }

    fn current_edges_for(&self, src: Uuid, predicate: &str) -> Result<Vec<RelationshipEdge>> {
        let id = uuid_bytes(src).to_vec();
        self.db.with_cog_conn(|conn| {
            // Case-insensitive on the predicate: extraction emits whatever
            // casing the model felt like, and `upsert_triplet` matches
            // existing edges the same way.
            let mut stmt = conn.prepare(
                "SELECT * FROM cog_edges
                 WHERE src = ?1 AND valid_to IS NULL
                   AND predicate = ?2 COLLATE NOCASE",
            )?;
            let rows = stmt
                .query_map(params![id, predicate], row_to_edge)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn edge_history(
        &self,
        src: Uuid,
        predicate: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RelationshipEdge>> {
        let id = uuid_bytes(src).to_vec();
        self.db.with_cog_conn(|conn| {
            let rows = match predicate {
                Some(p) => {
                    let mut stmt = conn.prepare(
                        "SELECT * FROM cog_edges
                         WHERE src = ?1 AND predicate = ?2 COLLATE NOCASE
                         ORDER BY valid_from ASC LIMIT ?3",
                    )?;
                    let out = stmt
                        .query_map(params![id, p, limit as i64], row_to_edge)?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    out
                }
                None => {
                    let mut stmt = conn.prepare(
                        "SELECT * FROM cog_edges
                         WHERE src = ?1 AND predicate NOT IN ('MENTIONS')
                         ORDER BY predicate ASC, valid_from ASC LIMIT ?2",
                    )?;
                    let out = stmt
                        .query_map(params![id, limit as i64], row_to_edge)?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    out
                }
            };
            Ok(rows)
        })
    }

    fn predicate_cardinality(&self, predicate: &str) -> Result<Cardinality> {
        let key = normalize_predicate(predicate);
        if key.is_empty() {
            return Ok(Cardinality::Multi);
        }
        self.db.with_cog_conn(|conn| {
            let found: Option<String> = conn
                .query_row(
                    "SELECT cardinality FROM cog_predicate_meta WHERE predicate = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(found.map(|s| Cardinality::parse(&s)).unwrap_or(Cardinality::Multi))
        })
    }

    fn set_predicate_cardinality(
        &self,
        predicate: &str,
        cardinality: Cardinality,
        source: &str,
    ) -> Result<()> {
        let key = normalize_predicate(predicate);
        if key.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp();
        self.db.with_cog_conn(|conn| {
            conn.execute(
                "INSERT INTO cog_predicate_meta (predicate, cardinality, source, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(predicate) DO UPDATE SET
                   cardinality = excluded.cardinality,
                   source      = excluded.source,
                   updated_at  = excluded.updated_at",
                params![key, cardinality.as_str(), source, now],
            )?;
            Ok(())
        })
    }

    fn list_predicate_meta(&self) -> Result<Vec<PredicateMetaRow>> {
        self.db.with_cog_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT predicate, cardinality, source, updated_at
                 FROM cog_predicate_meta ORDER BY cardinality, predicate",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(PredicateMetaRow {
                        predicate: r.get(0)?,
                        cardinality: r.get(1)?,
                        source: r.get(2)?,
                        updated_at: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn scan_edges(&self, limit: usize, offset: usize) -> Result<Vec<RelationshipEdge>> {
        self.db.with_cog_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT * FROM cog_edges
                 WHERE archived_at IS NULL
                 ORDER BY last_activated ASC
                 LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt
                .query_map(params![limit as i64, offset as i64], row_to_edge)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn count_edges(&self) -> Result<usize> {
        self.db.with_cog_conn(|conn| {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM cog_edges", [], |r| r.get(0))?;
            Ok(n as usize)
        })
    }

    fn record_decay_run(
        &self,
        run_at: i64,
        edges_scanned: usize,
        edges_pruned: usize,
        edges_promoted: usize,
        duration_ms: i64,
    ) -> Result<()> {
        self.db.with_cog_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO cog_decay_log
                 (run_at, edges_scanned, edges_pruned, edges_promoted, duration_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    run_at,
                    edges_scanned as i64,
                    edges_pruned as i64,
                    edges_promoted as i64,
                    duration_ms,
                ],
            )?;
            Ok(())
        })
    }

    fn tag_node(&self, node: Uuid, set: &NodeSet) -> Result<()> {
        let node_blob = uuid_bytes(node).to_vec();
        let now = chrono::Utc::now().timestamp();
        self.db.with_cog_conn(|conn| {
            // upsert the node_set, get its id
            conn.execute(
                "INSERT OR IGNORE INTO cog_node_sets (scope_kind, scope_id, tag, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![set.scope_kind.as_str(), set.scope_id, set.tag, now],
            )?;
            let set_id: i64 = conn.query_row(
                "SELECT id FROM cog_node_sets
                 WHERE scope_kind = ?1 AND scope_id = ?2 AND tag = ?3",
                params![set.scope_kind.as_str(), set.scope_id, set.tag],
                |r| r.get(0),
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO cog_node_tags (node_id, node_set_id) VALUES (?1, ?2)",
                params![node_blob, set_id],
            )?;
            Ok(())
        })
    }

    fn list_nodes(
        &self,
        kind: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<DataPoint>> {
        self.db.with_cog_conn(|conn| {
            if let Some(k) = kind {
                let mut stmt = conn.prepare(
                    "SELECT * FROM cog_nodes WHERE kind = ?1
                     ORDER BY last_seen_at DESC LIMIT ?2 OFFSET ?3",
                )?;
                let rows: Vec<DataPoint> = stmt
                    .query_map(params![k, limit as i64, offset as i64], row_to_node)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            } else {
                let mut stmt = conn.prepare(
                    "SELECT * FROM cog_nodes
                     ORDER BY last_seen_at DESC LIMIT ?1 OFFSET ?2",
                )?;
                let rows: Vec<DataPoint> = stmt
                    .query_map(params![limit as i64, offset as i64], row_to_node)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            }
        })
    }

    fn count_nodes(&self, kind: Option<&str>) -> Result<usize> {
        self.db.with_cog_conn(|conn| {
            let n: i64 = if let Some(k) = kind {
                conn.query_row(
                    "SELECT COUNT(*) FROM cog_nodes WHERE kind = ?1",
                    params![k],
                    |r| r.get(0),
                )?
            } else {
                conn.query_row("SELECT COUNT(*) FROM cog_nodes", [], |r| r.get(0))?
            };
            Ok(n as usize)
        })
    }

    fn edges_from_set(
        &self,
        set: &NodeSet,
        min_strength: f32,
        require_ltp: bool,
        limit: usize,
    ) -> Result<Vec<(RelationshipEdge, DataPoint, DataPoint)>> {
        self.db.with_cog_conn(|conn| {
            // Edges whose `src` is tagged with this NodeSet. We skip the
            // MENTIONS provenance predicate — those tie chunks to entities
            // and aren't statements *about* the agent in a way SOUL.md
            // should consume.
            let mut stmt = conn.prepare(
                "SELECT e.* FROM cog_edges e
                 JOIN cog_node_tags t ON t.node_id = e.src
                 JOIN cog_node_sets s ON s.id = t.node_set_id
                 WHERE s.scope_kind = ?1 AND s.scope_id = ?2 AND s.tag = ?3
                   AND e.predicate <> 'MENTIONS'
                   AND e.strength >= ?4
                   AND (?5 = 0 OR e.ltp_status > 0)
                 ORDER BY e.strength DESC, e.activation_count DESC
                 LIMIT ?6",
            )?;
            let raw_rows: Vec<RelationshipEdge> = stmt
                .query_map(
                    params![
                        set.scope_kind.as_str(),
                        set.scope_id,
                        set.tag,
                        min_strength as f64,
                        require_ltp as i64,
                        limit as i64,
                    ],
                    row_to_edge,
                )?
                .collect::<rusqlite::Result<_>>()?;
            drop(stmt);
            // Resolve src + dst nodes for each edge so the caller can
            // format readable bullets without a second round-trip.
            let mut out = Vec::with_capacity(raw_rows.len());
            for edge in raw_rows {
                let src_blob = uuid_bytes(edge.src).to_vec();
                let dst_blob = uuid_bytes(edge.dst).to_vec();
                let src: Option<DataPoint> = conn
                    .query_row(
                        "SELECT * FROM cog_nodes WHERE id = ?1",
                        params![src_blob],
                        row_to_node,
                    )
                    .optional()?;
                let dst: Option<DataPoint> = conn
                    .query_row(
                        "SELECT * FROM cog_nodes WHERE id = ?1",
                        params![dst_blob],
                        row_to_node,
                    )
                    .optional()?;
                if let (Some(s), Some(d)) = (src, dst) {
                    out.push((edge, s, d));
                }
            }
            Ok(out)
        })
    }

    fn reset_orphan_done_chunks(&self) -> Result<usize> {
        self.db.with_cog_conn(|conn| {
            let n = conn.execute(
                "UPDATE cog_nodes
                 SET extraction_state = 0
                 WHERE kind = 'chunk'
                   AND extraction_state = 1
                   AND id NOT IN (SELECT src FROM cog_edges)",
                [],
            )?;
            Ok(n)
        })
    }

    fn sets_of_node(&self, node: Uuid) -> Result<Vec<NodeSet>> {
        let node_blob = uuid_bytes(node).to_vec();
        self.db.with_cog_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT s.scope_kind, s.scope_id, s.tag
                 FROM cog_node_tags t
                 JOIN cog_node_sets s ON s.id = t.node_set_id
                 WHERE t.node_id = ?1",
            )?;
            let rows: Vec<NodeSet> = stmt
                .query_map(params![node_blob], |r| {
                    Ok(NodeSet {
                        scope_kind: super::node_set::ScopeKind::from_str(&r.get::<_, String>(0)?),
                        scope_id: r.get(1)?,
                        tag: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn cleanup_junk(&self) -> Result<CleanupReport> {
        self.db.with_cog_conn(|conn| {
            // Envelope-wrapped chunks. The patterns mirror what the
            // sanitize_for_cognify helper rejects at ingest time, applied
            // here retroactively to legacy data. We use LIKE not regex so
            // SQLite doesn't need a compiled extension.
            let envelope_chunks_removed = conn.execute(
                "DELETE FROM cog_nodes
                 WHERE kind = 'chunk'
                   AND (
                     summary LIKE '%<messages>%'
                     OR summary LIKE '%<message %'
                     OR summary LIKE '%</message>%'
                     OR summary LIKE '%<think>%'
                   )",
                [],
            )?;

            // Markup-heavy chunks: retroactive parity with the runtime
            // sanitizer. The 40%-markup ratio needs char counting, so this
            // pass scans in Rust instead of SQL. Envelope rows are already
            // gone from the pass above, keeping this scan small.
            let mut stmt =
                conn.prepare("SELECT id, summary FROM cog_nodes WHERE kind = 'chunk'")?;
            let chunk_rows: Vec<(Vec<u8>, String)> = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            let mut markup_chunks_removed = 0usize;
            for (id, summary) in chunk_rows {
                if super::cognify::sanitize_for_cognify(&summary).is_none() {
                    markup_chunks_removed +=
                        conn.execute("DELETE FROM cog_nodes WHERE id = ?1", params![id])?;
                }
            }

            // Meaningless entity names: no alphanumeric character at all
            // (pure punctuation/symbols/whitespace). Pure-digit names are
            // deliberately kept — "2026"/"8" can be real objects in date
            // triplets like (SemaClaw, deadline, 2026). Rust-side scan for
            // correct Unicode handling across scripts.
            let mut stmt = conn.prepare("SELECT id, name FROM cog_nodes WHERE kind = 'entity'")?;
            let entity_rows: Vec<(Vec<u8>, String)> = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            let mut junk_entities_removed = 0usize;
            for (id, name) in entity_rows {
                if !name.chars().any(|c| c.is_alphanumeric()) {
                    junk_entities_removed +=
                        conn.execute("DELETE FROM cog_nodes WHERE id = ?1", params![id])?;
                }
            }

            // Orphan entities: no incident edges. These accrue when an
            // upstream chunk was deleted (forget) but the entities it
            // produced stayed behind, or when an early cognify run was
            // partially aborted.
            let orphan_entities_removed = conn.execute(
                "DELETE FROM cog_nodes
                 WHERE kind = 'entity'
                   AND id NOT IN (SELECT src FROM cog_edges UNION SELECT dst FROM cog_edges)",
                [],
            )?;

            // Type-only entities: every incident edge points at an
            // entity_type node — i.e. the LLM emitted a typed entity but no
            // chunk MENTIONS it and no semantic relation grounds it.
            // Zero-edge entities are already gone (pass above), so this
            // counts strictly the `is_a`-only artifacts.
            let typeonly_entities_removed = conn.execute(
                "DELETE FROM cog_nodes
                 WHERE kind = 'entity'
                   AND id NOT IN (
                     SELECT e.src FROM cog_edges e
                       JOIN cog_nodes n ON n.id = e.dst
                      WHERE n.kind <> 'entity_type'
                     UNION
                     SELECT e.dst FROM cog_edges e
                       JOIN cog_nodes n ON n.id = e.src
                      WHERE n.kind <> 'entity_type'
                   )",
                [],
            )?;

            // entity_type nodes orphaned by the cascades above.
            let orphan_type_nodes_removed = conn.execute(
                "DELETE FROM cog_nodes
                 WHERE kind = 'entity_type'
                   AND id NOT IN (SELECT src FROM cog_edges UNION SELECT dst FROM cog_edges)",
                [],
            )?;

            Ok(CleanupReport {
                envelope_chunks_removed,
                markup_chunks_removed,
                junk_entities_removed,
                orphan_entities_removed,
                typeonly_entities_removed,
                orphan_type_nodes_removed,
            })
        })
    }

    fn merge_duplicate_entities(&self) -> Result<MergeReport> {
        self.db.with_cog_conn(|conn| {
            // Find groups of entity nodes sharing a normalised name. We
            // exclude empty names so chunks/auto-generated nodes don't get
            // collapsed.
            let mut stmt = conn.prepare(
                "SELECT LOWER(TRIM(name)) AS norm, COUNT(*) AS c
                 FROM cog_nodes
                 WHERE kind = 'entity' AND TRIM(name) <> ''
                 GROUP BY norm
                 HAVING c > 1",
            )?;
            let group_keys: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);

            let now = chrono::Utc::now().timestamp();
            let mut groups_merged = 0usize;
            let mut entities_merged = 0usize;
            let mut edges_redirected = 0usize;

            for norm in group_keys {
                // Members ranked by canonical-preference: highest mention,
                // then oldest. First row is the survivor.
                let mut q = conn.prepare(
                    "SELECT id, mention_count
                     FROM cog_nodes
                     WHERE kind = 'entity' AND LOWER(TRIM(name)) = ?1
                     ORDER BY mention_count DESC, created_at ASC",
                )?;
                let members: Vec<(Vec<u8>, i64)> = q
                    .query_map([&norm], |r| {
                        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(q);

                if members.len() < 2 {
                    continue;
                }
                let (canonical_id, _) = &members[0];
                let mut group_edges_kept = 0usize;
                let mut mention_sum: i64 = 0;

                for (dup_id, dup_mentions) in members.iter().skip(1) {
                    mention_sum += *dup_mentions;
                    group_edges_kept += redirect_edges_and_delete(conn, canonical_id, dup_id)?;
                    entities_merged += 1;
                }

                // Roll up the merged mention counts onto the survivor and
                // bump last_seen.
                conn.execute(
                    "UPDATE cog_nodes
                     SET mention_count = mention_count + ?2,
                         last_seen_at = ?3,
                         updated_at   = ?3
                     WHERE id = ?1",
                    rusqlite::params![canonical_id, mention_sum, now],
                )?;

                edges_redirected += group_edges_kept;
                groups_merged += 1;
            }

            Ok(MergeReport {
                groups_merged,
                entities_merged,
                edges_redirected,
            })
        })
    }

    fn top_entity_names(&self, set: Option<&NodeSet>, limit: usize) -> Result<Vec<String>> {
        self.db.with_cog_conn(|conn| {
            let names: Vec<String> = match set {
                Some(s) => {
                    let mut stmt = conn.prepare(
                        "SELECT n.name FROM cog_nodes n
                         JOIN cog_node_tags t ON t.node_id = n.id
                         JOIN cog_node_sets s ON s.id = t.node_set_id
                         WHERE s.scope_kind = ?1 AND s.scope_id = ?2 AND s.tag = ?3
                           AND n.kind = 'entity' AND TRIM(n.name) <> ''
                         ORDER BY n.mention_count DESC, n.last_seen_at DESC
                         LIMIT ?4",
                    )?;
                    let rows = stmt
                        .query_map(
                            params![s.scope_kind.as_str(), s.scope_id, s.tag, limit as i64],
                            |r| r.get::<_, String>(0),
                        )?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    rows
                }
                None => {
                    let mut stmt = conn.prepare(
                        "SELECT name FROM cog_nodes
                         WHERE kind = 'entity' AND TRIM(name) <> ''
                         ORDER BY mention_count DESC, last_seen_at DESC
                         LIMIT ?1",
                    )?;
                    let rows = stmt
                        .query_map(params![limit as i64], |r| r.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    rows
                }
            };
            Ok(names)
        })
    }

    fn merge_alias_entities(
        &self,
        min_cosine: f32,
        max_candidates: usize,
    ) -> Result<AliasMergeReport> {
        use super::vector_store::{blob_to_floats, cosine_distance};

        self.db.with_cog_conn(|conn| {
            struct Cand {
                id: Vec<u8>,
                name: String,
                type_name: String,
                mentions: i64,
                emb: Vec<f32>,
            }
            // Ordered by canonical preference (mentions desc, oldest first)
            // so cands[i] always wins over cands[j] when i < j.
            let mut stmt = conn.prepare(
                "SELECT id, name, type_name, mention_count, embedding
                 FROM cog_nodes
                 WHERE kind = 'entity' AND embedding IS NOT NULL
                   AND TRIM(name) <> ''
                 ORDER BY mention_count DESC, created_at ASC
                 LIMIT ?1",
            )?;
            let cands: Vec<Cand> = stmt
                .query_map(params![max_candidates as i64], |r| {
                    Ok(Cand {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        type_name: r.get::<_, String>(2)?.trim().to_lowercase(),
                        mentions: r.get(3)?,
                        emb: blob_to_floats(&r.get::<_, Vec<u8>>(4)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);

            let now = chrono::Utc::now().timestamp();
            let mut report = AliasMergeReport::default();
            let mut absorbed = vec![false; cands.len()];

            for i in 0..cands.len() {
                if absorbed[i] || cands[i].emb.is_empty() {
                    continue;
                }
                for j in (i + 1)..cands.len() {
                    if absorbed[j]
                        || cands[j].emb.is_empty()
                        || cands[i].type_name != cands[j].type_name
                        || cands[i].emb.len() != cands[j].emb.len()
                    {
                        continue;
                    }
                    report.pairs_examined += 1;
                    let sim = 1.0 - cosine_distance(&cands[i].emb, &cands[j].emb);
                    if sim < min_cosine {
                        continue;
                    }
                    report.edges_redirected +=
                        redirect_edges_and_delete(conn, &cands[i].id, &cands[j].id)?;
                    conn.execute(
                        "UPDATE cog_nodes
                         SET mention_count = mention_count + ?2,
                             last_seen_at = ?3, updated_at = ?3
                         WHERE id = ?1",
                        params![cands[i].id, cands[j].mentions, now],
                    )?;
                    // An alias merge joins two *different* surface names —
                    // consolidate the absorbed name into the canonical's
                    // `aka` list so the information isn't lost.
                    if !cands[j].name.trim().is_empty()
                        && cands[i].name.trim().to_lowercase()
                            != cands[j].name.trim().to_lowercase()
                    {
                        record_alias(conn, &cands[i].id, cands[j].name.trim())?;
                    }
                    absorbed[j] = true;
                    report.entities_merged += 1;
                }
            }
            Ok(report)
        })
    }

    fn infer_associative_edges(
        &self,
        min_cooccurrence: usize,
        max_per_run: usize,
    ) -> Result<InferenceReport> {
        self.db.with_cog_conn(|conn| {
            let now = chrono::Utc::now().timestamp();
            let min_cooc = min_cooccurrence.max(2) as i64;
            let cap = max_per_run.max(1) as i64;

            // Candidate co-occurring entity pairs: two entities reached by
            // `MENTIONS` from the same chunk. `e1.dst < e2.dst` makes each
            // unordered pair appear once (BLOB comparison is memcmp — stable
            // and good enough for dedup). `cooc` = how many distinct chunks
            // mention both. We only count it as a candidate when there is no
            // edge already connecting them in either direction.
            let candidates_examined: usize = conn.query_row(
                "SELECT COUNT(*) FROM (
                     SELECT e1.dst AS a, e2.dst AS b, COUNT(DISTINCT e1.src) AS cooc
                     FROM cog_edges e1
                     JOIN cog_edges e2
                       ON e1.src = e2.src AND e1.dst < e2.dst
                     WHERE e1.predicate = 'MENTIONS' AND e2.predicate = 'MENTIONS'
                     GROUP BY a, b
                     HAVING cooc >= ?1
                   ) pairs
                 WHERE NOT EXISTS (
                     SELECT 1 FROM cog_edges x
                     WHERE (x.src = pairs.a AND x.dst = pairs.b)
                        OR (x.src = pairs.b AND x.dst = pairs.a)
                   )",
                rusqlite::params![min_cooc],
                |r| r.get::<_, i64>(0),
            )? as usize;

            // Materialise the strongest candidates. Strength ramps with
            // co-occurrence but stays modest (max 0.5) so an inferred guess
            // never outranks an extracted fact; tier 0 (L1Working) means it
            // decays unless retrieval reinforces it. `props_json.inferred`
            // and the context string mark provenance for the UI / audits.
            let associations_created = conn.execute(
                "INSERT OR IGNORE INTO cog_edges
                    (src, dst, predicate, props_json,
                     valid_from, valid_to,
                     strength, tier, activation_count, last_activated,
                     ltp_status, ltp_detected_at,
                     entity_confidence, endpoint_selectivity, forman_curvature,
                     activation_timestamps,
                     source_episode_id, context, created_at)
                 SELECT a, b, 'ASSOCIATED_WITH', '{\"inferred\":true}',
                        ?2, NULL,
                        MIN(0.5, 0.15 + 0.05 * cooc), 0, 0, ?2,
                        0, NULL,
                        NULL, NULL, NULL,
                        '[]',
                        NULL, 'inferred:co-occurrence', ?2
                 FROM (
                     SELECT e1.dst AS a, e2.dst AS b, COUNT(DISTINCT e1.src) AS cooc
                     FROM cog_edges e1
                     JOIN cog_edges e2
                       ON e1.src = e2.src AND e1.dst < e2.dst
                     WHERE e1.predicate = 'MENTIONS' AND e2.predicate = 'MENTIONS'
                     GROUP BY a, b
                     HAVING cooc >= ?1
                 ) pairs
                 WHERE NOT EXISTS (
                     SELECT 1 FROM cog_edges x
                     WHERE (x.src = pairs.a AND x.dst = pairs.b)
                        OR (x.src = pairs.b AND x.dst = pairs.a)
                   )
                 ORDER BY cooc DESC
                 LIMIT ?3",
                rusqlite::params![min_cooc, now, cap],
            )?;

            Ok(InferenceReport {
                candidates_examined,
                associations_created,
            })
        })
    }

    fn full_graph(
        &self,
        node_limit: usize,
        edge_limit: usize,
        include_chunks: bool,
    ) -> Result<(Vec<DataPoint>, Vec<RelationshipEdge>)> {
        self.db.with_cog_conn(|conn| {
            let kind_filter = if include_chunks {
                ""
            } else {
                "WHERE kind != 'chunk'"
            };
            let node_sql = format!(
                "SELECT * FROM cog_nodes {kind_filter}
                 ORDER BY last_seen_at DESC LIMIT ?1"
            );
            let mut nstmt = conn.prepare(&node_sql)?;
            let nodes: Vec<DataPoint> = nstmt
                .query_map(params![node_limit as i64], row_to_node)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let id_set: std::collections::HashSet<Uuid> = nodes.iter().map(|n| n.id).collect();
            let mut estmt = conn.prepare(
                "SELECT * FROM cog_edges
                 ORDER BY strength DESC LIMIT ?1",
            )?;
            let all_edges: Vec<RelationshipEdge> = estmt
                .query_map(params![edge_limit as i64], row_to_edge)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let edges: Vec<RelationshipEdge> = all_edges
                .into_iter()
                .filter(|e| id_set.contains(&e.src) && id_set.contains(&e.dst))
                .collect();
            Ok((nodes, edges))
        })
    }

    fn top_nodes_by_degree(&self, limit: usize) -> Result<Vec<NodeWithDegree>> {
        self.db.with_cog_conn(|conn| {
            // Degree = count of incident edges in cog_edges (src or dst).
            // We pre-aggregate per-node via UNION ALL so SQLite can use the
            // (src) and (dst) indexes; doing OR in the WHERE clause forces
            // a full scan and is slow on big graphs.
            let mut stmt = conn.prepare(
                "WITH deg AS (
                   SELECT src AS id, COUNT(*) AS c FROM cog_edges GROUP BY src
                   UNION ALL
                   SELECT dst AS id, COUNT(*) AS c FROM cog_edges GROUP BY dst
                 ), totals AS (
                   SELECT id, SUM(c) AS degree FROM deg GROUP BY id
                 )
                 SELECT n.*, COALESCE(t.degree, 0) AS degree
                 FROM cog_nodes n
                 LEFT JOIN totals t ON t.id = n.id
                 ORDER BY degree DESC, n.last_seen_at DESC
                 LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(params![limit as i64], |row| {
                    let node = row_to_node(row)?;
                    let degree: i64 = row.get("degree")?;
                    Ok(NodeWithDegree {
                        node,
                        degree: degree as usize,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn recent_decay_runs(&self, limit: usize) -> Result<Vec<DecayLogRow>> {
        self.db.with_cog_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT run_at, edges_scanned, edges_pruned, edges_promoted, duration_ms
                 FROM cog_decay_log
                 ORDER BY run_at DESC
                 LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(params![limit as i64], |row| {
                    Ok(DecayLogRow {
                        run_at: row.get(0)?,
                        edges_scanned: row.get::<_, i64>(1)? as usize,
                        edges_pruned: row.get::<_, i64>(2)? as usize,
                        edges_promoted: row.get::<_, i64>(3)? as usize,
                        duration_ms: row.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn nodes_in_set(&self, set: &NodeSet, limit: usize) -> Result<Vec<DataPoint>> {
        self.db.with_cog_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT n.* FROM cog_nodes n
                 JOIN cog_node_tags t ON t.node_id = n.id
                 JOIN cog_node_sets s ON s.id = t.node_set_id
                 WHERE s.scope_kind = ?1 AND s.scope_id = ?2 AND s.tag = ?3
                 ORDER BY n.last_seen_at DESC
                 LIMIT ?4",
            )?;
            let rows = stmt
                .query_map(
                    params![set.scope_kind.as_str(), set.scope_id, set.tag, limit as i64],
                    row_to_node,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn edges_within_set(&self, set: &NodeSet, limit: usize) -> Result<Vec<RelationshipEdge>> {
        self.db.with_cog_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT e.* FROM cog_edges e
                 JOIN cog_node_tags ts ON ts.node_id = e.src
                 JOIN cog_node_sets ss ON ss.id = ts.node_set_id
                 JOIN cog_node_tags td ON td.node_id = e.dst
                 JOIN cog_node_sets sd ON sd.id = td.node_set_id
                 WHERE ss.scope_kind = ?1 AND ss.scope_id = ?2 AND ss.tag = ?3
                   AND sd.scope_kind = ?1 AND sd.scope_id = ?2 AND sd.tag = ?3
                 ORDER BY e.last_activated DESC
                 LIMIT ?4",
            )?;
            let rows = stmt
                .query_map(
                    params![set.scope_kind.as_str(), set.scope_id, set.tag, limit as i64],
                    row_to_edge,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    fn node_ids_in_sets(&self, sets: &[NodeSet]) -> Result<std::collections::HashSet<Uuid>> {
        let mut out = std::collections::HashSet::new();
        if sets.is_empty() {
            return Ok(out);
        }
        self.db.with_cog_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT t.node_id FROM cog_node_tags t
                 JOIN cog_node_sets s ON s.id = t.node_set_id
                 WHERE s.scope_kind = ?1 AND s.scope_id = ?2 AND s.tag = ?3",
            )?;
            for set in sets {
                let ids = stmt
                    .query_map(
                        params![set.scope_kind.as_str(), set.scope_id, set.tag],
                        |row| row.get::<_, Vec<u8>>(0),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for blob in ids {
                    if let Ok(u) = Uuid::from_slice(&blob) {
                        out.insert(u);
                    }
                }
            }
            Ok(())
        })?;
        Ok(out)
    }

    fn list_node_sets(&self) -> Result<Vec<NodeSetInfo>> {
        self.db.with_cog_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT s.scope_kind, s.scope_id, s.tag, COUNT(t.node_id) AS n
                 FROM cog_node_sets s
                 LEFT JOIN cog_node_tags t ON t.node_set_id = s.id
                 GROUP BY s.id
                 ORDER BY n DESC",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(NodeSetInfo {
                        scope_kind: row.get(0)?,
                        scope_id: row.get(1)?,
                        tag: row.get(2)?,
                        nodes: row.get::<_, i64>(3)? as usize,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;

    fn test_db() -> Arc<Db> {
        let cfg = Config::from_env();
        Arc::new(Db::open_in_memory(&cfg).expect("open in-memory db"))
    }

    #[test]
    fn node_sets_isolate_spaces() {
        let store = SqliteGraphStore::new(test_db());
        let a = DataPoint::entity("Alpha", 100);
        let b = DataPoint::entity("Beta", 100);
        store.upsert_node(&a).unwrap();
        store.upsert_node(&b).unwrap();
        let space_a = NodeSet::space("ai-office:nghien-cuu");
        let space_b = NodeSet::space("ai-office:noi-dung");
        store.tag_node(a.id, &space_a).unwrap();
        store.tag_node(b.id, &space_b).unwrap();

        // Membership is per-space — no leakage between the two.
        let ids_a = store.node_ids_in_sets(&[space_a.clone()]).unwrap();
        assert!(ids_a.contains(&a.id) && !ids_a.contains(&b.id));
        let ids_both = store.node_ids_in_sets(&[space_a, space_b.clone()]).unwrap();
        assert_eq!(ids_both.len(), 2);
        assert!(store.node_ids_in_sets(&[]).unwrap().is_empty());

        // Registry lists both spaces with one member each.
        let sets = store.list_node_sets().unwrap();
        let get = |id: &str| sets.iter().find(|s| s.scope_id == id).unwrap().nodes;
        assert_eq!(get("ai-office:nghien-cuu"), 1);
        assert_eq!(get("ai-office:noi-dung"), 1);
    }

    /// Two entities plus an edge between them, at a chosen assertion time.
    fn fact(store: &SqliteGraphStore, subj: &DataPoint, object: &str, pred: &str, at: i64) -> Uuid {
        let obj = DataPoint::entity(object, at);
        store.upsert_node(&obj).unwrap();
        let mut e = RelationshipEdge::new(subj.id, obj.id, pred, at);
        e.last_activated = at;
        store.upsert_edge(&e).unwrap();
        obj.id
    }

    #[test]
    fn superseded_facts_disappear_from_the_present_but_not_from_history() {
        let store = SqliteGraphStore::new(test_db());
        let shop = DataPoint::entity("BTMC", 0);
        store.upsert_node(&shop).unwrap();

        let old = fact(&store, &shop, "149.900", "sell_price", 1_000);
        let new = fact(&store, &shop, "141.500", "sell_price", 2_000);

        // Close the old one the way cognify does.
        let mut prior = store
            .current_edges_for(shop.id, "sell_price")
            .unwrap()
            .into_iter()
            .find(|e| e.dst == old)
            .unwrap();
        prior.invalidate(2_000, Uuid::new_v4());
        store.upsert_edge(&prior).unwrap();

        // "What is the price?" — one answer, the current one.
        let current = store.current_edges_for(shop.id, "sell_price").unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].dst, new);

        // Retrieval sees only the current fact...
        let now_edges = store.neighbors_at(shop.id, 10, None).unwrap();
        assert_eq!(now_edges.len(), 1, "superseded fact must not be recalled");
        assert_eq!(now_edges[0].dst, new);

        // ...but the old one is still there when you ask about back then.
        let then = store.neighbors_at(shop.id, 10, Some(1_500)).unwrap();
        assert_eq!(then.len(), 1);
        assert_eq!(then[0].dst, old, "as_of must return the fact of that time");

        // And nothing was deleted.
        assert_eq!(store.neighbors(shop.id, 10).unwrap().len(), 2);
        let history = store.edge_history(shop.id, Some("sell_price"), 50).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].dst, old, "history is oldest-first");
        assert!(history[0].valid_to.is_some());
        assert!(history[1].is_current());
    }

    // The half-open interval matters at the seam: at the exact second of the
    // handover, the new fact is the answer — otherwise both would match.
    #[test]
    fn the_supersession_instant_belongs_to_the_new_fact() {
        let store = SqliteGraphStore::new(test_db());
        let shop = DataPoint::entity("BTMC", 0);
        store.upsert_node(&shop).unwrap();
        let old = fact(&store, &shop, "149.900", "sell_price", 1_000);
        fact(&store, &shop, "141.500", "sell_price", 2_000);
        let mut prior = store
            .neighbors(shop.id, 10)
            .unwrap()
            .into_iter()
            .find(|e| e.dst == old)
            .unwrap();
        prior.invalidate(2_000, Uuid::new_v4());
        store.upsert_edge(&prior).unwrap();

        let at_seam = store.neighbors_at(shop.id, 10, Some(2_000)).unwrap();
        assert_eq!(at_seam.len(), 1);
        assert_ne!(at_seam[0].dst, old);
        // One second earlier the old fact was still in force.
        assert_eq!(
            store.neighbors_at(shop.id, 10, Some(1_999)).unwrap()[0].dst,
            old
        );
    }

    // Dormant is not false: decay must not hide a fact from recall.
    #[test]
    fn archived_edges_are_still_current_facts() {
        let store = SqliteGraphStore::new(test_db());
        let a = DataPoint::entity("A", 0);
        store.upsert_node(&a).unwrap();
        let b = fact(&store, &a, "B", "lives_in", 1_000);

        let mut e = store.neighbors(a.id, 10).unwrap().remove(0);
        e.archive(5_000);
        store.upsert_edge(&e).unwrap();

        let current = store.neighbors_at(a.id, 10, None).unwrap();
        assert_eq!(current.len(), 1, "an archived fact is still the answer");
        assert_eq!(current[0].dst, b);
        assert!(current[0].is_archived() && current[0].is_current());
        // But decay stops scanning it.
        assert!(store.scan_edges(10, 0).unwrap().is_empty());
    }

    #[test]
    fn predicate_cardinality_defaults_to_multi_and_is_overridable() {
        let store = SqliteGraphStore::new(test_db());
        // Seeded by the schema.
        assert_eq!(
            store.predicate_cardinality("sell_price").unwrap(),
            Cardinality::Single
        );
        assert_eq!(
            store.predicate_cardinality("has_task").unwrap(),
            Cardinality::Multi
        );
        // Casing/spacing an LLM might emit still resolves.
        assert_eq!(
            store.predicate_cardinality("Sell Price").unwrap(),
            Cardinality::Single
        );
        // Never-seen predicate: multi, so nothing gets superseded by accident.
        assert_eq!(
            store.predicate_cardinality("wibbles_at").unwrap(),
            Cardinality::Multi
        );

        store
            .set_predicate_cardinality("wibbles_at", Cardinality::Single, "user")
            .unwrap();
        assert_eq!(
            store.predicate_cardinality("wibbles_at").unwrap(),
            Cardinality::Single
        );
        let row = store
            .list_predicate_meta()
            .unwrap()
            .into_iter()
            .find(|r| r.predicate == "wibbles_at")
            .unwrap();
        assert_eq!(row.source, "user", "hand edits must be marked as such");
    }

    #[test]
    fn upsert_and_get_node_roundtrip() {
        let store = SqliteGraphStore::new(test_db());
        let node = DataPoint::entity("Ada Lovelace", 100);
        store.upsert_node(&node).unwrap();
        let fetched = store.get_node(node.id).unwrap().expect("node exists");
        assert_eq!(fetched.name, "Ada Lovelace");
        assert_eq!(fetched.kind, NodeKind::Entity);
        assert!(fetched.is_proper_noun);
    }

    #[test]
    fn upsert_and_query_edge() {
        let store = SqliteGraphStore::new(test_db());
        let a = DataPoint::entity("Alice", 1);
        let b = DataPoint::entity("Bob", 1);
        store.upsert_node(&a).unwrap();
        store.upsert_node(&b).unwrap();

        let mut edge = RelationshipEdge::new(a.id, b.id, "knows", 1);
        edge.strength = 0.5;
        store.upsert_edge(&edge).unwrap();

        let nbrs = store.neighbors(a.id, 10).unwrap();
        assert_eq!(nbrs.len(), 1);
        assert_eq!(nbrs[0].predicate, "knows");
        assert!((nbrs[0].strength - 0.5).abs() < 1e-3);
    }

    #[test]
    fn node_set_tagging_and_lookup() {
        let store = SqliteGraphStore::new(test_db());
        let chunk = DataPoint::chunk("hello world", Some("abc".into()), 10);
        store.upsert_node(&chunk).unwrap();

        let set = NodeSet::group("group_jid_1", "default_memory");
        store.tag_node(chunk.id, &set).unwrap();

        let nodes = store.nodes_in_set(&set, 10).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, chunk.id);
    }

    #[test]
    fn sets_of_node_reverses_tagging() {
        let store = SqliteGraphStore::new(test_db());
        let chunk = DataPoint::chunk("hello", Some("h".into()), 10);
        store.upsert_node(&chunk).unwrap();

        let g = NodeSet::group("jid-1", "default_memory");
        let space = NodeSet::space("ai-chat:support");
        store.tag_node(chunk.id, &g).unwrap();
        store.tag_node(chunk.id, &space).unwrap();

        let sets = store.sets_of_node(chunk.id).unwrap();
        assert_eq!(sets.len(), 2);
        assert!(sets.contains(&g));
        assert!(sets.contains(&space));

        // Untagged node → empty, not an error.
        let bare = DataPoint::chunk("bare", Some("h2".into()), 10);
        store.upsert_node(&bare).unwrap();
        assert!(store.sets_of_node(bare.id).unwrap().is_empty());
    }

    #[test]
    fn reset_orphan_done_chunks_targets_only_edgeless_done() {
        use crate::memory::cognitive::ExtractionState as S;
        let store = SqliteGraphStore::new(test_db());

        // Done + edge-less → reset to Pending (its facts decayed away).
        let lost = DataPoint::chunk("facts long gone", Some("h1".into()), 1);
        store.upsert_node(&lost).unwrap();
        store.set_extraction_state(lost.id, S::Done, 2).unwrap();

        // Done + still has a MENTIONS edge → untouched.
        let kept = DataPoint::chunk("facts intact", Some("h2".into()), 1);
        let ent = DataPoint::entity("Ada", 1);
        store.upsert_node(&kept).unwrap();
        store.upsert_node(&ent).unwrap();
        store.set_extraction_state(kept.id, S::Done, 2).unwrap();
        store
            .upsert_edge(&RelationshipEdge::new(kept.id, ent.id, "MENTIONS", 2))
            .unwrap();

        // SkippedNoFacts + edge-less → untouched (LLM said "nothing here";
        // decay didn't destroy anything).
        let nofacts = DataPoint::chunk("small talk only", Some("h3".into()), 1);
        store.upsert_node(&nofacts).unwrap();
        store
            .set_extraction_state(nofacts.id, S::SkippedNoFacts, 2)
            .unwrap();

        let reset = store.reset_orphan_done_chunks().unwrap();
        assert_eq!(reset, 1, "only the Done-but-edgeless chunk resets");

        let states: Vec<S> = [lost.id, kept.id, nofacts.id]
            .iter()
            .map(|id| store.get_node(*id).unwrap().unwrap().extraction_state)
            .collect();
        assert_eq!(states[0], S::Pending);
        assert_eq!(states[1], S::Done);
        assert_eq!(states[2], S::SkippedNoFacts);
    }

    #[test]
    fn dedupe_by_content_hash() {
        let store = SqliteGraphStore::new(test_db());
        let chunk = DataPoint::chunk("payload", Some("hash-1".into()), 1);
        store.upsert_node(&chunk).unwrap();
        let dup = store.find_node_by_content_hash("hash-1").unwrap();
        assert!(dup.is_some());
        assert_eq!(dup.unwrap().id, chunk.id);
    }

    #[test]
    fn top_nodes_by_degree_orders_correctly() {
        // hub --(rel)--> a, b, c   (degree 3)
        // a   --(rel)--> b         (degree 2)
        // b                         (degree 2 from above)
        // c                         (degree 1)
        // → expected ordering by degree desc: hub, a, b, c
        let store = SqliteGraphStore::new(test_db());
        let hub = DataPoint::entity("hub", 1);
        let a = DataPoint::entity("a", 1);
        let b = DataPoint::entity("b", 1);
        let c = DataPoint::entity("c", 1);
        let lonely = DataPoint::entity("lonely", 1);
        for n in [&hub, &a, &b, &c, &lonely] {
            store.upsert_node(n).unwrap();
        }
        let mk = |src, dst| {
            let mut e = RelationshipEdge::new(src, dst, "rel", 1);
            e.last_activated = 1;
            e
        };
        store.upsert_edge(&mk(hub.id, a.id)).unwrap();
        store.upsert_edge(&mk(hub.id, b.id)).unwrap();
        store.upsert_edge(&mk(hub.id, c.id)).unwrap();
        store.upsert_edge(&mk(a.id, b.id)).unwrap();

        let top = store.top_nodes_by_degree(10).unwrap();
        // 5 nodes, ordered by degree desc
        assert_eq!(top.len(), 5);
        assert_eq!(top[0].node.name, "hub");
        assert_eq!(top[0].degree, 3);
        // a and b both have degree 2 — order between them depends on
        // last_seen_at tiebreaker, but both must precede c (deg 1).
        let degrees: Vec<usize> = top.iter().map(|x| x.degree).collect();
        assert_eq!(degrees, vec![3, 2, 2, 1, 0]);
        // The lonely node lands last with degree 0.
        assert_eq!(top[4].node.name, "lonely");
        assert_eq!(top[4].degree, 0);
    }

    #[test]
    fn find_entity_by_name_is_case_insensitive_and_trimmed() {
        let store = SqliteGraphStore::new(test_db());
        let ada = DataPoint::entity("Ada Lovelace", 100);
        store.upsert_node(&ada).unwrap();

        // Different casing + surrounding whitespace still resolves to the
        // same node (so ingest reuses it instead of creating a duplicate).
        for variant in ["ada lovelace", "ADA LOVELACE", "  Ada Lovelace  "] {
            let hit = store.find_entity_by_name(variant).unwrap();
            assert_eq!(
                hit.map(|n| n.id),
                Some(ada.id),
                "variant {variant:?} should resolve to the canonical entity"
            );
        }

        // A genuinely different name does not match.
        assert!(store
            .find_entity_by_name("Charles Babbage")
            .unwrap()
            .is_none());
    }

    #[test]
    fn fts_search_nodes_finds_filters_and_removes() {
        let store = SqliteGraphStore::new(test_db());

        let ada = DataPoint::entity("Ada Lovelace", 100);
        store.upsert_node(&ada).unwrap();
        let mut chunk = DataPoint::chunk("Ada designed an early compiler", Some("h1".into()), 1);
        chunk.id = uuid::Uuid::new_v4();
        store.upsert_node(&chunk).unwrap();

        // Matches across both kinds; scores normalised into [0, 1].
        let hits = store.fts_search_nodes("ada", None, 10).unwrap();
        assert!(hits.iter().any(|(n, _)| n.id == ada.id));
        assert!(hits.iter().any(|(n, _)| n.id == chunk.id));
        assert!(hits.iter().all(|(_, s)| (0.0..=1.0).contains(s)));

        // Kind filter keeps only the entity.
        let ents = store.fts_search_nodes("ada", Some("entity"), 10).unwrap();
        assert!(!ents.is_empty());
        assert!(ents.iter().all(|(n, _)| n.kind == NodeKind::Entity));
        assert!(ents.iter().any(|(n, _)| n.id == ada.id));

        // Empty match string short-circuits.
        assert!(store.fts_search_nodes("  ", None, 10).unwrap().is_empty());

        // Delete cascades to the FTS index via trigger.
        store.delete_node(ada.id).unwrap();
        let after = store.fts_search_nodes("lovelace", None, 10).unwrap();
        assert!(after.iter().all(|(n, _)| n.id != ada.id));
    }

    #[test]
    fn cleanup_junk_removes_envelope_chunks_and_orphans() {
        let store = SqliteGraphStore::new(test_db());

        // Junk chunk #1 — envelope wrapper.
        let mut junk = DataPoint::chunk(
            "<messages><message sender=\"x\" time=\"t\">hi</message></messages>",
            Some("h1".into()),
            1,
        );
        junk.id = uuid::Uuid::new_v4();
        store.upsert_node(&junk).unwrap();

        // Junk chunk #2 — has a `<message ` tag mid-text.
        let mut junk2 = DataPoint::chunk(
            "prefix <message sender=\"a\">x</message>",
            Some("h2".into()),
            1,
        );
        junk2.id = uuid::Uuid::new_v4();
        store.upsert_node(&junk2).unwrap();

        // Good chunk — plain sentence, kept.
        let mut good = DataPoint::chunk("Ada invented the compiler", Some("h3".into()), 1);
        good.id = uuid::Uuid::new_v4();
        store.upsert_node(&good).unwrap();

        // Orphan entity — no edges incident.
        let mut orphan = DataPoint::entity("OrphanEntity", 1);
        orphan.id = uuid::Uuid::new_v4();
        store.upsert_node(&orphan).unwrap();

        // Connected entity — has incoming edge from `good`.
        let mut connected = DataPoint::entity("ConnectedEntity", 1);
        connected.id = uuid::Uuid::new_v4();
        store.upsert_node(&connected).unwrap();
        let mut e = RelationshipEdge::new(good.id, connected.id, "MENTIONS", 1);
        e.last_activated = 1;
        store.upsert_edge(&e).unwrap();

        let report = store.cleanup_junk().unwrap();
        assert_eq!(report.envelope_chunks_removed, 2);
        assert_eq!(report.orphan_entities_removed, 1);

        // Survivors only.
        assert!(store.get_node(good.id).unwrap().is_some());
        assert!(store.get_node(connected.id).unwrap().is_some());
        assert!(store.get_node(junk.id).unwrap().is_none());
        assert!(store.get_node(junk2.id).unwrap().is_none());
        assert!(store.get_node(orphan.id).unwrap().is_none());
    }

    #[test]
    fn cleanup_junk_removes_markup_chunks_and_meaningless_entities() {
        let store = SqliteGraphStore::new(test_db());

        // Markup-heavy chunk — >40% angle-bracket chars, no envelope tag,
        // so only the retroactive sanitize pass catches it.
        let mut markup = DataPoint::chunk(
            "<a><b><c><d><e><f><g><h>x</h></g></f></e></d></c></b></a>",
            Some("m1".into()),
            1,
        );
        markup.id = uuid::Uuid::new_v4();
        store.upsert_node(&markup).unwrap();

        // Good chunk mentioning a good entity.
        let mut good = DataPoint::chunk("SemaClaw runs on Rust", Some("m2".into()), 1);
        good.id = uuid::Uuid::new_v4();
        store.upsert_node(&good).unwrap();

        // Symbol-only entity name → junk. Digit-only name → kept.
        let mut symbols = DataPoint::entity("###---", 1);
        symbols.id = uuid::Uuid::new_v4();
        store.upsert_node(&symbols).unwrap();
        let mut year = DataPoint::entity("2026", 1);
        year.id = uuid::Uuid::new_v4();
        store.upsert_node(&year).unwrap();

        // Grounded entity: MENTIONS from the good chunk.
        let mut grounded = DataPoint::entity("SemaClaw", 1);
        grounded.id = uuid::Uuid::new_v4();
        store.upsert_node(&grounded).unwrap();

        // Type-only entity: sole edge is `is_a → entity_type`.
        let mut typeonly = DataPoint::entity("GhostEntity", 1);
        typeonly.id = uuid::Uuid::new_v4();
        store.upsert_node(&typeonly).unwrap();
        let type_node = DataPoint::entity_type("person", 1);
        store.upsert_node(&type_node).unwrap();

        let mk = |src, dst, pred: &str| {
            let mut e = RelationshipEdge::new(src, dst, pred, 1);
            e.last_activated = 1;
            e
        };
        store
            .upsert_edge(&mk(good.id, grounded.id, "MENTIONS"))
            .unwrap();
        // Keep `year` and `symbols` edge-connected so only the name/type
        // passes (not the orphan pass) can be what removes them.
        store
            .upsert_edge(&mk(grounded.id, year.id, "deadline"))
            .unwrap();
        store
            .upsert_edge(&mk(grounded.id, symbols.id, "rel"))
            .unwrap();
        store
            .upsert_edge(&mk(typeonly.id, type_node.id, "is_a"))
            .unwrap();

        let report = store.cleanup_junk().unwrap();
        assert_eq!(report.envelope_chunks_removed, 0);
        assert_eq!(report.markup_chunks_removed, 1);
        assert_eq!(report.junk_entities_removed, 1, "symbol-only entity");
        assert_eq!(report.typeonly_entities_removed, 1, "is_a-only entity");
        assert_eq!(
            report.orphan_type_nodes_removed, 1,
            "type node orphaned once GhostEntity fell"
        );
        assert_eq!(report.total_removed(), 4);

        // Survivors: good chunk, grounded entity, digit-named entity.
        assert!(store.get_node(good.id).unwrap().is_some());
        assert!(store.get_node(grounded.id).unwrap().is_some());
        assert!(store.get_node(year.id).unwrap().is_some());
        assert!(store.get_node(markup.id).unwrap().is_none());
        assert!(store.get_node(symbols.id).unwrap().is_none());
        assert!(store.get_node(typeonly.id).unwrap().is_none());
        assert!(store.get_node(type_node.id).unwrap().is_none());
    }

    /// Cleanup must never eat a plain short chat chunk — the sanitize pass
    /// rejects only markup-heavy or sub-10-char text.
    #[test]
    fn cleanup_junk_keeps_ordinary_prose_chunks() {
        let store = SqliteGraphStore::new(test_db());
        let mut vi = DataPoint::chunk("tôi tên là Sen, sống ở Hà Nội", Some("p1".into()), 1);
        vi.id = uuid::Uuid::new_v4();
        store.upsert_node(&vi).unwrap();
        let mut cmp = DataPoint::chunk("giá < 100k -> mua ngay", Some("p2".into()), 1);
        cmp.id = uuid::Uuid::new_v4();
        store.upsert_node(&cmp).unwrap();

        let report = store.cleanup_junk().unwrap();
        assert_eq!(report.envelope_chunks_removed, 0);
        assert_eq!(report.markup_chunks_removed, 0);
        assert!(store.get_node(vi.id).unwrap().is_some());
        assert!(store.get_node(cmp.id).unwrap().is_some());
    }

    #[test]
    fn top_entity_names_orders_by_mentions_and_scopes_to_set() {
        let store = SqliteGraphStore::new(test_db());
        let mut a = DataPoint::entity("SemaClaw", 1);
        a.mention_count = 9;
        let mut b = DataPoint::entity("Hà Nội", 1);
        b.mention_count = 3;
        let mut c = DataPoint::entity("Untagged", 1);
        c.mention_count = 99;
        for n in [&a, &b, &c] {
            store.upsert_node(n).unwrap();
        }
        let set = NodeSet::group("g1", "default_memory");
        store.tag_node(a.id, &set).unwrap();
        store.tag_node(b.id, &set).unwrap();

        // Global: highest mentions first, includes untagged.
        let all = store.top_entity_names(None, 10).unwrap();
        assert_eq!(all, vec!["Untagged", "SemaClaw", "Hà Nội"]);

        // Scoped: only the group's entities.
        let scoped = store.top_entity_names(Some(&set), 10).unwrap();
        assert_eq!(scoped, vec!["SemaClaw", "Hà Nội"]);
    }

    #[test]
    fn merge_alias_entities_merges_similar_same_type_only() {
        use crate::memory::cognitive::vector_store::{SqliteVectorStore, VectorStore};

        let db = test_db();
        let store = SqliteGraphStore::new(Arc::clone(&db));
        let vectors = SqliteVectorStore::new(db);

        // Canonical: more mentions. Alias: nearly identical embedding,
        // same type. Distinct: same type but orthogonal embedding.
        // Lookalike: near embedding but DIFFERENT type — must survive.
        let mut hanoi = DataPoint::entity("Hà Nội", 1);
        hanoi.type_name = "city".into();
        hanoi.mention_count = 5;
        let mut hn = DataPoint::entity("HN", 2);
        hn.type_name = "city".into();
        hn.mention_count = 1;
        let mut paris = DataPoint::entity("Paris", 1);
        paris.type_name = "city".into();
        let mut lookalike = DataPoint::entity("Hanoi Corp", 1);
        lookalike.type_name = "organization".into();
        for n in [&hanoi, &hn, &paris, &lookalike] {
            store.upsert_node(n).unwrap();
        }
        vectors.upsert(hanoi.id, &[1.0, 0.0, 0.0], "m").unwrap();
        vectors.upsert(hn.id, &[0.99, 0.05, 0.0], "m").unwrap();
        vectors.upsert(paris.id, &[0.0, 1.0, 0.0], "m").unwrap();
        vectors
            .upsert(lookalike.id, &[0.99, 0.05, 0.0], "m")
            .unwrap();

        // Ground the alias with an edge so redirect has work to do.
        let mut chunk = DataPoint::chunk("HN là thủ đô", Some("h1".into()), 1);
        chunk.id = uuid::Uuid::new_v4();
        store.upsert_node(&chunk).unwrap();
        let mut e = RelationshipEdge::new(chunk.id, hn.id, "MENTIONS", 1);
        e.last_activated = 1;
        store.upsert_edge(&e).unwrap();

        let report = store.merge_alias_entities(0.9, 100).unwrap();
        assert_eq!(report.entities_merged, 1, "only HN merges into Hà Nội");
        assert_eq!(report.edges_redirected, 1);

        // Alias gone, canonical rolled up, others intact.
        assert!(store.get_node(hn.id).unwrap().is_none());
        let canon = store.get_node(hanoi.id).unwrap().expect("canonical");
        assert_eq!(canon.mention_count, 6, "mentions rolled up");
        assert!(store.get_node(paris.id).unwrap().is_some());
        assert!(store.get_node(lookalike.id).unwrap().is_some());
        // Redirected MENTIONS edge now lands on the canonical.
        let nbrs = store.neighbors(chunk.id, 10).unwrap();
        assert!(nbrs
            .iter()
            .any(|e| e.dst == hanoi.id && e.predicate == "MENTIONS"));
        // The absorbed surface name is consolidated into `aka`, not lost.
        let aka: Vec<String> = canon
            .props
            .get("aka")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert!(aka.iter().any(|s| s == "HN"), "aka must record HN: {aka:?}");
    }

    /// The space-scoped graph view returns only edges with BOTH endpoints
    /// inside the space — crossing edges stay out.
    #[test]
    fn edges_within_set_returns_only_internal_edges() {
        let store = SqliteGraphStore::new(test_db());
        let a = DataPoint::entity("A", 1);
        let b = DataPoint::entity("B", 1);
        let outside = DataPoint::entity("Outside", 1);
        for n in [&a, &b, &outside] {
            store.upsert_node(n).unwrap();
        }
        let space = NodeSet::space("ai-chat:support");
        store.tag_node(a.id, &space).unwrap();
        store.tag_node(b.id, &space).unwrap();

        let mk = |src, dst, pred: &str| {
            let mut e = RelationshipEdge::new(src, dst, pred, 1);
            e.last_activated = 1;
            e
        };
        store.upsert_edge(&mk(a.id, b.id, "supports")).unwrap();
        store.upsert_edge(&mk(a.id, outside.id, "crosses")).unwrap();

        let edges = store.edges_within_set(&space, 100).unwrap();
        assert_eq!(edges.len(), 1, "only the internal edge: {edges:?}");
        assert_eq!(edges[0].predicate, "supports");
    }

    /// A merge must consolidate the duplicate's information onto the
    /// canonical — space tags union, summary adopted — never drop it.
    #[test]
    fn merge_preserves_space_tags_and_summary() {
        let store = SqliteGraphStore::new(test_db());

        let mut canon = DataPoint::entity("Acme", 1);
        canon.mention_count = 5;
        let mut dup = DataPoint::entity("acme", 2);
        dup.mention_count = 1;
        dup.summary = "công ty phần mềm ở Hà Nội".into();
        let other = DataPoint::entity("Bob", 1);
        for n in [&canon, &dup, &other] {
            store.upsert_node(n).unwrap();
        }

        // Canonical lives in space A; duplicate lives in space B. After the
        // merge the canonical must be a member of BOTH.
        let space_a = NodeSet::group("space-a", "default_memory");
        let space_b = NodeSet::group("space-b", "default_memory");
        store.tag_node(canon.id, &space_a).unwrap();
        store.tag_node(dup.id, &space_b).unwrap();

        // Ground both so cleanup-order concerns don't apply and redirect
        // has an edge to move.
        let mut e1 = RelationshipEdge::new(other.id, canon.id, "knows", 1);
        e1.last_activated = 1;
        store.upsert_edge(&e1).unwrap();
        let mut e2 = RelationshipEdge::new(other.id, dup.id, "works_at", 1);
        e2.last_activated = 1;
        store.upsert_edge(&e2).unwrap();

        let rep = store.merge_duplicate_entities().unwrap();
        assert_eq!(rep.entities_merged, 1);

        // Space membership is the union of both.
        let sets = store.sets_of_node(canon.id).unwrap();
        assert!(
            sets.iter().any(|s| s.scope_id == "space-a")
                && sets.iter().any(|s| s.scope_id == "space-b"),
            "canonical must belong to both spaces after merge: {sets:?}"
        );
        // The duplicate's summary was adopted (canonical had none).
        let merged = store.get_node(canon.id).unwrap().expect("canonical");
        assert_eq!(merged.summary, "công ty phần mềm ở Hà Nội");
    }
}

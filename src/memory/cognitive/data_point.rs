//! DataPoint — node payload in the cognitive graph.
//!
//! Port of cognee `DataPoint` + shodh `EntityNode`. A DataPoint is the unit
//! of memory: an entity, a chunk, a summary, or a user-defined type.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Entity,
    /// Category/class of an entity (cognee `EntityType`). Entities point at
    /// their type via an `is_a` edge. Kept distinct from `Entity` so type
    /// nodes never get picked up by `find_entity_by_name` (which keys on
    /// `kind = 'entity'`) — otherwise "person" the type would shadow
    /// "person" the entity.
    EntityType,
    Chunk,
    Summary,
    Custom,
}

impl NodeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::EntityType => "entity_type",
            Self::Chunk => "chunk",
            Self::Summary => "summary",
            Self::Custom => "custom",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "entity" => Self::Entity,
            "entity_type" => Self::EntityType,
            "chunk" => Self::Chunk,
            "summary" => Self::Summary,
            _ => Self::Custom,
        }
    }
}

/// Triplet-extraction state for chunk nodes. Persisted on `cog_nodes`
/// so the cognify pipeline can decide whether to call the LLM again
/// without inferring it from "does this chunk have edges?". Cheaper to
/// query (one column read vs. neighbor scan) and unambiguous (the
/// `skipped_no_facts` case has zero edges by definition but should NOT
/// be retried — the LLM has already told us there's nothing to extract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i64)]
pub enum ExtractionState {
    Pending = 0,
    Done = 1,
    /// LLM call itself failed (no client wired, network error, …).
    /// Cognify should retry next time an LLM is available.
    SkippedNoLlm = 2,
    /// LLM ran but produced no usable triplets (parse error, empty
    /// triplets array). Don't retry by default — the LLM has already
    /// decided this chunk has nothing extract-worthy. User can force a
    /// retry via the DataPoints "re-extract" button.
    SkippedNoFacts = 3,
}

impl ExtractionState {
    pub fn from_i64(v: i64) -> Self {
        match v {
            1 => Self::Done,
            2 => Self::SkippedNoLlm,
            3 => Self::SkippedNoFacts,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataPoint {
    pub id: Uuid,
    pub kind: NodeKind,
    /// User-defined type name (e.g. `"ScientificPaper"`) — only meaningful
    /// when `kind == Custom`. Empty string otherwise.
    pub type_name: String,
    pub name: String,
    pub summary: String,
    /// Content hash (e.g. SHA-256 hex) for dedupe.
    pub content_hash: Option<String>,
    pub props: Value,
    /// shodh dynamics
    pub salience: f32,
    pub mention_count: u32,
    pub is_proper_noun: bool,
    pub selectivity: Option<f32>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seen_at: i64,
    /// Persisted extraction state — `Pending` for fresh chunks, advanced
    /// by [`super::cognify::CognifyPipeline`] after each LLM attempt.
    /// Entity / summary nodes are derived artefacts → always `Done`.
    pub extraction_state: ExtractionState,
    /// Unix-seconds of the last extraction attempt (success OR skip).
    /// `None` for chunks that have never been processed.
    pub extracted_at: Option<i64>,
}

impl DataPoint {
    /// New chunk node (most common path during `add()`).
    pub fn chunk(text: impl Into<String>, content_hash: Option<String>, now: i64) -> Self {
        let text = text.into();
        Self {
            id: Uuid::new_v4(),
            kind: NodeKind::Chunk,
            type_name: String::new(),
            name: String::new(),
            summary: text,
            content_hash,
            props: Value::Object(Default::default()),
            salience: 0.5,
            mention_count: 1,
            is_proper_noun: false,
            selectivity: None,
            created_at: now,
            updated_at: now,
            last_seen_at: now,
            extraction_state: ExtractionState::Pending,
            extracted_at: None,
        }
    }

    /// New entity node (produced by `cognify` / triplet extraction).
    ///
    /// Starts untyped; the cognify pipeline sets `type_name` (the cognee-style
    /// category, e.g. `"person"` / `"city"`) and wires an `is_a` edge to the
    /// matching [`DataPoint::entity_type`] node when the extractor supplied a
    /// type. `type_name` is the denormalised copy so a single node read tells
    /// you the type without traversing the edge.
    pub fn entity(name: impl Into<String>, now: i64) -> Self {
        let name = name.into();
        let proper = name.chars().next().is_some_and(|c| c.is_uppercase());
        Self {
            id: Uuid::new_v4(),
            kind: NodeKind::Entity,
            type_name: String::new(),
            name,
            summary: String::new(),
            content_hash: None,
            props: Value::Object(Default::default()),
            salience: 0.5,
            mention_count: 1,
            is_proper_noun: proper,
            selectivity: None,
            created_at: now,
            updated_at: now,
            last_seen_at: now,
            // Entities are produced BY extraction — there's nothing to
            // extract from them.
            extraction_state: ExtractionState::Done,
            extracted_at: Some(now),
        }
    }

    /// New entity-type (category) node. The id is a deterministic UUIDv5 of
    /// the canonical name so repeated extraction of the same type dedupes
    /// for free via `ON CONFLICT(id)` — no name lookup needed, mirroring
    /// cognee's `uuid5`-keyed `EntityType` nodes.
    pub fn entity_type(name: impl Into<String>, now: i64) -> Self {
        let name = name.into();
        let id = Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("entity_type:{name}").as_bytes(),
        );
        Self {
            id,
            kind: NodeKind::EntityType,
            type_name: String::new(),
            name,
            summary: String::new(),
            content_hash: None,
            props: Value::Object(Default::default()),
            salience: 0.5,
            mention_count: 1,
            is_proper_noun: false,
            selectivity: None,
            created_at: now,
            updated_at: now,
            last_seen_at: now,
            extraction_state: ExtractionState::Done,
            extracted_at: Some(now),
        }
    }
}

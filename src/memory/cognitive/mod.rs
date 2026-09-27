//! Cognitive memory layer — hybrid of cognee (graph extraction + retrieval)
//! and shodh-memory (Hebbian dynamics + LTP + tiered consolidation).
//!
//! ## P1 scope (this commit)
//!
//! Skeleton only: schema, types, `GraphStore` trait + SQLite impl. No
//! ingestion pipeline, no retrievers, no MCP tool surface yet — those land
//! in P3 / P4 per the integration plan.
//!
//! ## Module map
//!
//! * [`schema`]      — SQLite DDL (cog_nodes, cog_edges, cog_node_sets, …)
//! * [`data_point`]  — node payload (entity / chunk / summary / custom)
//! * [`triplet`]     — `RelationshipEdge` with Hebbian `strengthen` + `decay`
//! * [`tiers`]       — `EdgeTier::{L1Working, L2Episodic, L3Semantic}`
//! * [`ltp`]         — Long-term potentiation state machine
//! * [`node_set`]    — scope tagging (group / persona / cowork / global)
//! * [`predicate_meta`] — per-predicate cardinality (drives fact supersession)
//! * [`graph_store`] — `GraphStore` trait + `SqliteGraphStore`

pub mod cognify;
pub mod data_point;
pub mod decay_tick;
pub mod embed;
pub mod extract;
pub mod gnn;
pub mod gnn_sage;
pub mod graph_store;
pub mod llm;
pub mod llm_anthropic;
pub mod llm_openai;
pub mod ltp;
pub mod maintenance;
pub mod node_set;
pub mod predicate_meta;
pub mod retrievers;
pub mod schema;
pub mod search;
pub mod soul_editor;
pub mod soul_ingest;
pub mod system;
pub mod tiers;
pub mod triplet;
pub mod vector_store;

pub use cognify::{sanitize_for_cognify, CognifyOptions, CognifyPipeline, CognifyReport};
pub use data_point::{DataPoint, ExtractionState, NodeKind};
pub use decay_tick::{run_decay, start_decay_ticker, DecayConfig, DecayReport};
pub use embed::{embed_node, CognitiveEmbedder, NullEmbedder};
pub use extract::{extract_text, MAX_FILE_SIZE_BYTES};
pub use gnn::{GraphScorer, LightGcnScorer};
pub use gnn_sage::{
    forward_inference as sage_forward, train as sage_train, GraphSageScorer, SageModel,
    TrainConfig as SageTrainConfig, TrainReport as SageTrainReport,
    TrainingFixture as SageTrainingFixture,
};
pub use graph_store::{
    CleanupReport, DecayLogRow, GraphStore, InferenceReport, MergeReport, NodeSetInfo,
    NodeWithDegree, SqliteGraphStore,
};
pub use llm::{LlmClient, RawTriplet};
pub use llm_anthropic::AnthropicLlm;
pub use llm_openai::{create_cognitive_llm, OpenAiCompatLlm};
pub use ltp::{detect_ltp_status, LtpStatus};
pub use maintenance::{
    run_maintenance, start_maintenance_ticker, MaintenanceConfig, MaintenanceReport,
};
pub use node_set::{NodeSet, ScopeKind};
pub use predicate_meta::Cardinality;
pub use retrievers::CognitiveRetriever;
pub use search::{parse_as_of, SearchHit, SearchQuery, SearchType};
pub use soul_ingest::{
    consolidate_to_soul, ingest_all_souls, ingest_soul, ingest_soul_from_disk, spawn_soul_watcher,
    split_soul_sections, ConsolidateReport,
};
pub use system::{
    format_hits_for_prompt, init_daemon, shutdown_decay, try_get_instance, CognitiveStats,
    CognitiveSystem,
};
pub use tiers::EdgeTier;
pub use triplet::RelationshipEdge;
pub use vector_store::{SqliteVectorStore, VectorHit, VectorStore};

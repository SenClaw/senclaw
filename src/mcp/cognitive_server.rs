//! Cognitive memory MCP server — exposes the graph + Hebbian layer to agents.
//!
//! Tools:
//!   * `cog_add`           — ingest text into a chunk node (no LLM extraction)
//!   * `cog_cognify`       — full pipeline: chunk → triplets → graph
//!   * `cog_search`        — generic SearchType dispatch
//!   * `cog_recall`        — convenience for SpreadingActivation (recall w/ write-back)
//!   * `cog_history`       — timeline of one fact (every value + its window)
//!   * `cog_forget`        — delete a node (cascades edges)
//!   * `cog_memory_stats`  — counts + LTP histogram
//!
//! Design notes:
//!   * Mirrors the stdio reconstruct-per-call pattern from `memory_server`.
//!   * `cog_cognify` requires an LLM client. We support **two modes**:
//!     1. `SENCLAW_COG_LLM_DISABLED=1`   → `cog_cognify` returns a 400-style
//!        error. Other tools still work (text ingest, search).
//!     2. (default) use the bundled OpenAI-compatible LLM provider via
//!        [`mcp_llm::OpenAiCompatLlm`] — wired by env vars (see helper.rs).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::Config;
use crate::db::Db;
use crate::mcp::schedule_server::ToolResult;
use crate::memory::cognitive::{
    create_cognitive_llm, parse_as_of, CognifyOptions, CognitiveSystem, LlmClient, NodeSet,
    SearchQuery, SearchType,
};
use crate::memory::embedding::{create_embedding_provider, EmbeddingProvider};

use async_trait::async_trait;
use rmcp::ServiceExt;

// =====================================================================
// MCP wire schemas
// =====================================================================

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct AddParams {
    text: String,
    #[serde(default)]
    source: Option<String>,
    /// Optional node_set tags (free-form). Defaults to the active group scope.
    #[serde(default)]
    tags: Vec<String>,
    /// Optional knowledge space id — isolates this memory into a named space.
    #[serde(default)]
    space: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct CognifyParams {
    text: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    /// Optional knowledge space id — isolates this memory into a named space.
    #[serde(default)]
    space: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct SearchParams {
    query: String,
    /// One of: chunks | triplet | graph | spreading | fts | hybrid | temporal.
    /// Defaults to "graph". `fts` needs no embeddings; `hybrid` blends
    /// vector + FTS; `temporal` ranks facts by how close they sit to `as_of`.
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    hops: Option<u8>,
    /// Optional knowledge space id — restrict search to that space only.
    #[serde(default)]
    space: Option<String>,
    /// Answer as of this moment in time instead of now — "2026-07-31",
    /// "2026-07-31T09:00:00Z" or unix seconds. Facts superseded before it are
    /// returned; facts asserted after it are not. Omit for current facts.
    #[serde(default)]
    as_of: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct RecallParams {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    hops: Option<u8>,
    /// Optional knowledge space id — restrict recall to that space only.
    #[serde(default)]
    space: Option<String>,
    /// Recall what was true at this moment instead of now — see cog_search.
    #[serde(default)]
    as_of: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct HistoryParams {
    /// Entity name, as it appears in the graph (case-insensitive).
    subject: String,
    /// Restrict to one predicate, e.g. "sell_price". Omit for every fact
    /// about the subject.
    #[serde(default)]
    predicate: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ForgetParams {
    /// Node UUID as a hyphenated string.
    node_id: String,
}

// =====================================================================
// Stdio bridge — reconstructs CognitiveSystem from env on each call
// =====================================================================

#[derive(Clone)]
struct McpCognitiveServer {
    db_path: String,
    group_folder: String,
    llm_disabled: bool,
}

impl McpCognitiveServer {
    fn open_system(&self) -> Result<CognitiveSystem> {
        let mut cfg = Config::from_env();
        // Layer in Settings → Embedding UI choices so the stdio MCP path
        // sees the same provider the daemon picked. Without this, agents
        // would call cog_* against an env-only Config that ignores the UI.
        let gcp = cfg.paths.global_config_path.clone();
        cfg.apply_persisted_overrides(&gcp);
        let mut db_cfg = cfg.clone();
        db_cfg.paths.db_path = PathBuf::from(&self.db_path);
        let db = Arc::new(Db::open(&db_cfg).context("open cognitive DB")?);
        let provider_box = create_embedding_provider(&cfg, Arc::clone(&db))
            .ok_or_else(|| anyhow::anyhow!("no embedding provider configured"))?;
        let provider: Arc<dyn EmbeddingProvider> = Arc::from(provider_box);
        let llm: Arc<dyn LlmClient> = if self.llm_disabled {
            Arc::new(DisabledLlm)
        } else {
            create_cognitive_llm(&cfg).unwrap_or_else(|| Arc::new(DisabledLlm))
        };
        Ok(CognitiveSystem::with_sqlite(db, provider, llm))
    }

}

#[rmcp::tool_router(server_handler)]
impl McpCognitiveServer {
    #[rmcp::tool(
        description = "Ingest text into cognitive memory as a chunk node. Skips LLM triplet extraction."
    )]
    async fn cog_add(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            AddParams,
        >,
    ) -> String {
        match self.open_system() {
            Ok(sys) => {
                CognitiveServer::new(sys, self.group_folder.clone())
                    .cog_add(&p.text, p.source.as_deref(), &p.tags, p.space.as_deref())
                    .await
                    .content
            }
            Err(e) => ToolResult::err(format!("Error: {e}")).content,
        }
    }

    #[rmcp::tool(
        description = "Full cognify pipeline: chunk → LLM triplet extraction → upsert graph. Idempotent."
    )]
    async fn cog_cognify(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            CognifyParams,
        >,
    ) -> String {
        match self.open_system() {
            Ok(sys) => {
                CognitiveServer::new(sys, self.group_folder.clone())
                    .cog_cognify(&p.text, p.source.as_deref(), &p.tags, p.space.as_deref())
                    .await
                    .content
            }
            Err(e) => ToolResult::err(format!("Error: {e}")).content,
        }
    }

    #[rmcp::tool(
        description = "Search cognitive memory. mode: chunks | triplet | graph | spreading | fts | hybrid"
    )]
    async fn cog_search(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            SearchParams,
        >,
    ) -> String {
        match self.open_system() {
            Ok(sys) => {
                CognitiveServer::new(sys, self.group_folder.clone())
                    .cog_search(
                        &p.query,
                        p.mode.as_deref(),
                        p.limit,
                        p.hops,
                        p.space.as_deref(),
                        p.as_of.as_deref(),
                    )
                    .await
                    .content
            }
            Err(e) => ToolResult::err(format!("Error: {e}")).content,
        }
    }

    #[rmcp::tool(
        description = "Recall memories via spreading activation (with Hebbian write-back)."
    )]
    async fn cog_recall(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            RecallParams,
        >,
    ) -> String {
        match self.open_system() {
            Ok(sys) => {
                CognitiveServer::new(sys, self.group_folder.clone())
                    .cog_recall(
                        &p.query,
                        p.limit,
                        p.hops,
                        p.space.as_deref(),
                        p.as_of.as_deref(),
                    )
                    .await
                    .content
            }
            Err(e) => ToolResult::err(format!("Error: {e}")).content,
        }
    }

    #[rmcp::tool(
        description = "Timeline of a fact: every value a subject's predicate has held, with the window each was believed in. Use when the answer depends on when — 'what did X used to be', 'when did X change'."
    )]
    fn cog_history(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            HistoryParams,
        >,
    ) -> String {
        match self.open_system() {
            Ok(sys) => {
                CognitiveServer::new(sys, self.group_folder.clone())
                    .cog_history(&p.subject, p.predicate.as_deref(), p.limit)
                    .content
            }
            Err(e) => ToolResult::err(format!("Error: {e}")).content,
        }
    }

    #[rmcp::tool(description = "Delete a node and its edges from cognitive memory.")]
    async fn cog_forget(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            ForgetParams,
        >,
    ) -> String {
        match self.open_system() {
            Ok(sys) => {
                CognitiveServer::new(sys, self.group_folder.clone())
                    .cog_forget(&p.node_id)
                    .content
            }
            Err(e) => ToolResult::err(format!("Error: {e}")).content,
        }
    }

    #[rmcp::tool(description = "Return counts of nodes/edges in cognitive memory.")]
    fn cog_memory_stats(&self) -> String {
        match self.open_system() {
            Ok(sys) => {
                CognitiveServer::new(sys, self.group_folder.clone())
                    .cog_memory_stats()
                    .content
            }
            Err(e) => ToolResult::err(format!("Error: {e}")).content,
        }
    }
}

/// Stdio entry-point (`senclaw cognitive-server`).
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let db_path = std::env::var("SENCLAW_DB_PATH").context("SENCLAW_DB_PATH not set")?;
    let group_folder = std::env::var("SENCLAW_GROUP_FOLDER").unwrap_or_default();
    let llm_disabled = std::env::var("SENCLAW_COG_LLM_DISABLED").ok().as_deref() == Some("1");

    let server = McpCognitiveServer {
        db_path,
        group_folder,
        llm_disabled,
    };
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// =====================================================================
// Inner server — programmatic API, used both by stdio bridge and tests
// =====================================================================

/// Placeholder LLM that always errors. Cognify will surface this to the
/// caller. Replaced by a real client in P7.
struct DisabledLlm;
#[async_trait]
impl LlmClient for DisabledLlm {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
        anyhow::bail!("cog_cognify is disabled: no LLM client wired in this process")
    }
}

pub struct CognitiveServer {
    sys: CognitiveSystem,
    group_folder: String,
}

impl CognitiveServer {
    pub fn new(sys: CognitiveSystem, group_folder: String) -> Self {
        Self { sys, group_folder }
    }

    fn build_node_sets(&self, extra: &[String], space: Option<&str>) -> Vec<NodeSet> {
        let mut sets = Vec::with_capacity(extra.len() + 2);
        sets.push(NodeSet::group(&self.group_folder, "default_memory"));
        if let Some(space) = space.map(str::trim).filter(|s| !s.is_empty()) {
            sets.push(NodeSet::space(space));
        }
        for tag in extra {
            sets.push(NodeSet::group(&self.group_folder, tag));
        }
        sets
    }

    pub async fn cog_add(
        &self,
        text: &str,
        source: Option<&str>,
        tags: &[String],
        space: Option<&str>,
    ) -> ToolResult {
        // `cog_add` is the agent's "remember this" entry point. Originally we
        // shipped a chunk-only fast path here so it'd work even without an
        // LLM, but that turned out to be the wrong default: callers expect
        // sentences like "tôi tên là Sen" to produce (tôi)-[name]->(Sen)
        // edges, not an orphan chunk node. We now delegate to the full
        // cognify pipeline. When the LLM is missing the pipeline still
        // stores the chunk + embedding (triplet step fails silently per the
        // existing graceful-degradation in `extract_triplets`) so we don't
        // regress the no-LLM case.
        let opts = CognifyOptions {
            node_sets: self.build_node_sets(tags, space),
            ..Default::default()
        };
        match self
            .sys
            .cognify(text, source.unwrap_or("mcp:cog_add"), &opts)
            .await
        {
            Ok(r) => ToolResult::ok(format!(
                "{{\"chunks_added\":{},\"chunks_deduped\":{},\"entities_added\":{},\"entities_reused\":{},\"edges_added\":{},\"edges_strengthened\":{}}}",
                r.chunks_added, r.chunks_deduped, r.entities_added,
                r.entities_reused, r.edges_added, r.edges_strengthened
            )),
            Err(e) => ToolResult::err(format!("cog_add failed: {e}")),
        }
    }

    pub async fn cog_cognify(
        &self,
        text: &str,
        source: Option<&str>,
        tags: &[String],
        space: Option<&str>,
    ) -> ToolResult {
        let opts = CognifyOptions {
            node_sets: self.build_node_sets(tags, space),
            ..Default::default()
        };
        match self.sys.cognify(text, source.unwrap_or("mcp"), &opts).await {
            Ok(r) => ToolResult::ok(format!(
                "{{\"chunks_added\":{},\"chunks_deduped\":{},\"entities_added\":{},\"entities_reused\":{},\"edges_added\":{},\"edges_strengthened\":{}}}",
                r.chunks_added, r.chunks_deduped, r.entities_added,
                r.entities_reused, r.edges_added, r.edges_strengthened
            )),
            Err(e) => ToolResult::err(format!("cog_cognify failed: {e}")),
        }
    }

    pub async fn cog_search(
        &self,
        query: &str,
        mode: Option<&str>,
        limit: Option<usize>,
        hops: Option<u8>,
        space: Option<&str>,
        as_of: Option<&str>,
    ) -> ToolResult {
        let raw_mode = mode.unwrap_or("graph");
        let q_type = match raw_mode {
            "chunks" => SearchType::Chunks,
            "triplet" => SearchType::Triplet,
            "spreading" => SearchType::SpreadingActivation,
            "fts" => SearchType::Fts,
            "hybrid" => SearchType::Hybrid,
            "temporal" => SearchType::Temporal,
            _ => SearchType::GraphCompletion,
        };
        // An unparseable `as_of` must not silently answer about *now* — that
        // is a wrong answer wearing the right shape.
        let at = match as_of.map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => match parse_as_of(raw) {
                Some(t) => Some(t),
                None => {
                    return ToolResult::err(format!(
                        "cog_search: could not read as_of {raw:?} — use 2026-07-31, \
                         2026-07-31T09:00:00Z or unix seconds"
                    ))
                }
            },
            None => None,
        };
        let mut q = SearchQuery::chunks(query, limit.unwrap_or(8));
        q.query_type = q_type;
        q.hops = hops.unwrap_or(2);
        q.decay_per_hop = 0.6;
        q.as_of = at;
        if let Some(space) = space.map(str::trim).filter(|s| !s.is_empty()) {
            q.node_sets = vec![NodeSet::space(space)];
        }
        match self.sys.search(&q).await {
            Ok(hits) => format_hits(&hits),
            Err(e) => ToolResult::err(format!("cog_search failed: {e}")),
        }
    }

    /// Timeline of one fact — every value `(subject, predicate)` has held and
    /// the window each was believed in. This is the read side of supersession:
    /// nothing is deleted when a fact changes, so the history is always there
    /// to show.
    pub fn cog_history(
        &self,
        subject: &str,
        predicate: Option<&str>,
        limit: Option<usize>,
    ) -> ToolResult {
        let entity = match self.sys.graph.find_entity_by_name(subject.trim()) {
            Ok(Some(e)) => e,
            Ok(None) => {
                return ToolResult::ok(format!(
                    "No entity named {subject:?} in the graph."
                ))
            }
            Err(e) => return ToolResult::err(format!("cog_history failed: {e}")),
        };
        let rows = match self
            .sys
            .graph
            .edge_history(entity.id, predicate, limit.unwrap_or(50))
        {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("cog_history failed: {e}")),
        };
        if rows.is_empty() {
            return ToolResult::ok(format!("No recorded facts for {:?}.", entity.name));
        }
        let mut out = format!("History for {}:\n", entity.name);
        for e in &rows {
            let object = self
                .sys
                .graph
                .get_node(e.dst)
                .ok()
                .flatten()
                .map(|n| if n.name.is_empty() { n.summary } else { n.name })
                .unwrap_or_else(|| e.dst.to_string());
            let until = match e.valid_to {
                None => "now".to_string(),
                Some(t) => fmt_ts(t),
            };
            out.push_str(&format!(
                "- {} = {}  [{} → {}]{}\n",
                e.predicate,
                object,
                fmt_ts(e.valid_from),
                until,
                if e.is_current() { "  (current)" } else { "" }
            ));
        }
        ToolResult::ok(out)
    }

    pub async fn cog_recall(
        &self,
        query: &str,
        limit: Option<usize>,
        hops: Option<u8>,
        space: Option<&str>,
        as_of: Option<&str>,
    ) -> ToolResult {
        let mut q = SearchQuery::spreading(query, limit.unwrap_or(8), hops.unwrap_or(2));
        if let Some(raw) = as_of.map(str::trim).filter(|s| !s.is_empty()) {
            match parse_as_of(raw) {
                Some(t) => q.as_of = Some(t),
                None => {
                    return ToolResult::err(format!(
                        "cog_recall: could not read as_of {raw:?} — use 2026-07-31, \
                         2026-07-31T09:00:00Z or unix seconds"
                    ))
                }
            }
        }
        if let Some(space) = space.map(str::trim).filter(|s| !s.is_empty()) {
            q.node_sets = vec![NodeSet::space(space)];
        }
        match self.sys.search(&q).await {
            Ok(hits) => format_hits(&hits),
            Err(e) => ToolResult::err(format!("cog_recall failed: {e}")),
        }
    }

    pub fn cog_forget(&self, node_id: &str) -> ToolResult {
        let id = match uuid::Uuid::parse_str(node_id) {
            Ok(u) => u,
            Err(e) => return ToolResult::err(format!("invalid uuid: {e}")),
        };
        match self.sys.graph.delete_node(id) {
            Ok(_) => {
                let _ = self.sys.vector.delete(id);
                ToolResult::ok(format!("{{\"forgotten\":\"{node_id}\"}}"))
            }
            Err(e) => ToolResult::err(format!("cog_forget failed: {e}")),
        }
    }

    pub fn cog_memory_stats(&self) -> ToolResult {
        match self.sys.stats() {
            Ok(s) => ToolResult::ok(format!("{{\"edges\":{}}}", s.edges)),
            Err(e) => ToolResult::err(format!("cog_memory_stats failed: {e}")),
        }
    }
}

/// Human-readable UTC stamp for timelines. Deliberately not localised: this
/// text goes into an LLM prompt, where an unambiguous format beats a pretty
/// one.
fn fmt_ts(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| secs.to_string())
}

fn format_hits(hits: &[crate::memory::cognitive::SearchHit]) -> ToolResult {
    if hits.is_empty() {
        return ToolResult::ok("No matching memories found.".into());
    }
    let mut out = format!("Found {} results:\n\n", hits.len());
    for (i, h) in hits.iter().enumerate() {
        let label = if h.node.name.is_empty() {
            let body = if h.node.summary.len() > 200 {
                format!(
                    "{}...",
                    crate::util::text::truncate_on_char_boundary(&h.node.summary, 200)
                )
            } else {
                h.node.summary.clone()
            };
            body
        } else {
            h.node.name.clone()
        };
        out.push_str(&format!(
            "[{}] {} (score: {:.3}, kind: {})\n",
            i + 1,
            label,
            h.score,
            h.node.kind.as_str()
        ));
    }
    ToolResult::ok(out.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::cognitive::llm::test_support::StubLlm;

    fn build_server(replies: Vec<String>) -> CognitiveServer {
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
                        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
                        v.iter().map(|x| x / n).collect()
                    })
                    .collect())
            }
        }

        let cfg = Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbedder);
        let llm: Arc<dyn LlmClient> = Arc::new(StubLlm::new(replies));
        let sys = CognitiveSystem::with_sqlite(db, provider, llm);
        CognitiveServer::new(sys, "test-group".into())
    }

    #[tokio::test]
    async fn cog_add_then_search() {
        let srv = build_server(vec![]);
        let r = srv
            .cog_add("the compiler runs on the machine", None, &[], None)
            .await;
        assert!(!r.is_error, "{}", r.content);
        let s = srv
            .cog_search("compiler", Some("chunks"), Some(5), None, None, None)
            .await;
        assert!(!s.is_error);
        assert!(s.content.contains("compiler"), "got: {}", s.content);
    }

    #[tokio::test]
    async fn cog_add_dedupes_on_repeat() {
        let srv = build_server(vec![]);
        let r1 = srv.cog_add("identical payload", None, &[], None).await;
        let r2 = srv.cog_add("identical payload", None, &[], None).await;
        assert!(
            r2.content.contains("deduped"),
            "second add should dedupe, got: {}",
            r2.content
        );
        let _ = r1;
    }

    #[tokio::test]
    async fn cog_cognify_then_recall_writes_back() {
        let canned =
            r#"{"triplets":[{"subject":"Ada","predicate":"invented","object":"compiler"}]}"#
                .to_string();
        let srv = build_server(vec![canned]);
        let c = srv
            .cog_cognify("Ada invented the compiler.", None, &[], None)
            .await;
        assert!(!c.is_error, "{}", c.content);
        assert!(c.content.contains("entities_added"));

        let r = srv.cog_recall("compiler", Some(5), Some(2), None, None).await;
        assert!(!r.is_error);
        assert!(r.content.contains("Found") || r.content.contains("No matching"));
    }

    #[tokio::test]
    async fn cog_memory_stats_returns_json() {
        let srv = build_server(vec![]);
        let r = srv.cog_memory_stats();
        assert!(!r.is_error);
        assert!(r.content.starts_with("{\"edges\":"));
    }

    #[tokio::test]
    async fn cog_forget_rejects_bad_uuid() {
        let srv = build_server(vec![]);
        let r = srv.cog_forget("not-a-uuid");
        assert!(r.is_error);
        assert!(r.content.contains("invalid uuid"));
    }
}

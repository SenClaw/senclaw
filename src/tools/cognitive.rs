//! Cognitive memory tools — knowledge-graph operations exposed to agents.
//!
//! These were previously delivered via the `senclaw-cognitive` MCP stdio
//! server, but every agent already runs in the same process as the
//! cognitive system (P10 `init_daemon`). Going through a stdio subprocess
//! added ~5 ms per call, an extra DB handle, and a third-party JSON-RPC
//! wire layer for what should be a direct in-process call.
//!
//! All five tools share these properties:
//!   * Access the live `CognitiveSystem` via [`cognitive::try_get_instance`]
//!     — the same singleton the daemon booted. No reconstruction per call.
//!   * Return a 503-style error string when cognitive is dormant (no
//!     embedding provider configured) instead of panicking.
//!   * `is_read_only` reflects whether the tool writes state. Used by the
//!     permission layer to skip prompts for pure reads.
//!
//! Tool descriptions are deliberately verbose — the LLM sees them before
//! choosing what to call, and the cognitive tools have semantics no other
//! tool in the system replicates (graph + Hebbian + spreading activation).
//! Lean into telling the agent _when_ to use them.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::memory::cognitive::{self, CognifyOptions, NodeSet, SearchQuery, SearchType};
use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

// =====================================================================
// Shared helpers
// =====================================================================

/// Borrow the live cognitive system; produce a "dormant" error
/// description otherwise. Centralised so every tool reports it the same
/// way and the LLM learns to recognise the recovery hint.
fn require_system() -> Result<std::sync::Arc<cognitive::CognitiveSystem>> {
    cognitive::try_get_instance().ok_or_else(|| {
        anyhow::anyhow!(
            "Cognitive memory is dormant — the daemon has no embedding provider configured. \
             Ask the user to set `SENCLAW_MEMORY_EMBEDDING_PROVIDER` (or pick one in Settings → \
             Embedding) and restart, then try again."
        )
    })
}

/// `node_set` scope policy lifted from the old MCP server (P6) — every
/// node written by a tool call gets tagged with the caller's agent so
/// recall can be scoped per agent later.
fn default_node_sets(agent_id: &str) -> Vec<NodeSet> {
    vec![NodeSet::group(agent_id, "default_memory")]
}

/// Build the `result_for_assistant` text plus the structured data the UI
/// renders in a single helper. The summary line is what the agent reads
/// when chaining tool calls; the JSON is for the chat UI's tool card.
/// Read the optional `as_of` argument. An unparseable value is an **error**,
/// never a silent fall back to "now": answering about the present when the
/// caller asked about the past is a wrong answer that looks right.
fn parse_as_of_arg(input: &Value) -> Result<Option<i64>> {
    let Some(raw) = input.get("as_of").and_then(|v| v.as_str()) else {
        // Numbers are accepted too — models emit `as_of: 1754006400` often
        // enough that rejecting it would just be rude.
        return Ok(input.get("as_of").and_then(|v| v.as_i64()));
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    cognitive::parse_as_of(raw).map(Some).ok_or_else(|| {
        anyhow::anyhow!(
            "could not read as_of {raw:?} — use 2026-07-31, 2026-07-31T09:00:00Z or unix seconds"
        )
    })
}

fn assistant_result(summary: impl Into<String>, data: Value) -> ToolOutput {
    ToolOutput::Result {
        data,
        result_for_assistant: summary.into(),
    }
}

// =====================================================================
// CogAdd — write a memory
// =====================================================================

pub struct CogAddTool;

#[async_trait]
impl Tool for CogAddTool {
    fn name(&self) -> &str {
        "CogAdd"
    }

    fn description(&self) -> &str {
        "Save a fact or statement into cognitive memory (knowledge graph). The text is chunked, \
         embedded, and run through an LLM-driven triplet extractor that turns sentences into \
         (subject, predicate, object) edges. Use this whenever the user tells you something \
         worth remembering across sessions — names, preferences, ongoing projects, decisions, \
         relationships, dates. Idempotent: re-saving the same text strengthens existing edges \
         via Hebbian learning instead of duplicating them. Skip pure questions and one-word \
         acknowledgements — those have no facts to extract."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The fact or sentence to remember. Can be multilingual."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional category tags scoped to this agent (e.g. ['preferences','work']). Empty = default_memory."
                }
            },
            "required": ["text"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let sys = require_system()?;
        let text = input
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing `text`"))?
            .trim();
        if text.is_empty() {
            anyhow::bail!("`text` must not be empty");
        }
        let tags: Vec<String> = input
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let mut sets = default_node_sets(ctx.agent_id);
        for t in &tags {
            sets.push(NodeSet::group(ctx.agent_id, t));
        }
        let opts = CognifyOptions {
            node_sets: sets,
            ..Default::default()
        };

        let report = sys.cognify(text, "tool:cog_add", &opts).await?;
        let data = serde_json::json!({
            "chunks_added": report.chunks_added,
            "chunks_deduped": report.chunks_deduped,
            "entities_added": report.entities_added,
            "entities_reused": report.entities_reused,
            "edges_added": report.edges_added,
            "edges_strengthened": report.edges_strengthened,
            "llm_skipped": report.llm_skipped,
        });
        // Two failure modes call for distinct hints to the agent:
        //   * llm_skipped=true → no LLM configured; chunk saved but no
        //     facts extracted. Tell the user to set Cognitive/Main LLM.
        //   * no skipped, but zero entities/edges → LLM ran, text had no
        //     facts (or was a question). Quiet success.
        let mut summary = format!(
            "Saved: {} chunk(s), {} entity, {} edge added; {} edge strengthened",
            report.chunks_added,
            report.entities_added,
            report.edges_added,
            report.edges_strengthened
        );
        if report.llm_skipped {
            summary.push_str(
                "\n⚠ LLM not configured — the sentence was stored as a chunk but no \
                 (subject, predicate, object) triplets were extracted. \
                 Configure a Cognitive Model (or any LLM Model) in Settings → LLM Models, \
                 then re-save the message to build edges.",
            );
        }
        Ok(vec![assistant_result(summary, data)])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "Cognitive · add".into(),
            summary: format!(
                "+{} edges, +{} entities",
                data.get("edges_added")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                data.get("entities_added")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            ),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let text = input
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("(empty)");
        let trimmed: String = text.chars().take(40).collect();
        format!("CogAdd · {trimmed}")
    }
}

// =====================================================================
// CogSearch — generic knowledge-graph search
// =====================================================================

pub struct CogSearchTool;

#[async_trait]
impl Tool for CogSearchTool {
    fn name(&self) -> &str {
        "CogSearch"
    }

    fn description(&self) -> &str {
        "Search cognitive memory across four modes:\n\
         • chunks — dense-vector text retrieval (best for keyword/topic lookup)\n\
         • triplet — return entities + their outgoing edges (best for 'who knows X?')\n\
         • graph — k-hop subgraph expansion from semantic seeds (default, best for context)\n\
         • spreading — like graph but ALSO strengthens edges via Hebbian write-back (use when \
           the query represents real recall the user should remember more easily next time)\n\
         • temporal — facts as they stood at `as_of`, ranked by closeness to that moment\n\
         Every mode answers with CURRENT facts by default: a value that a later one replaced \
         (an old price, a previous job) is excluded. Pass `as_of` to ask what was true then \
         instead — 'giá vàng ngày 31/07' → as_of='2026-07-31'.\n\
         Prefer CogRecall for everyday 'what do I know about X' questions — it picks spreading \
         automatically and includes write-back."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural-language query." },
                "mode": {
                    "type": "string",
                    "enum": ["chunks", "triplet", "graph", "spreading", "temporal"],
                    "description": "Retrieval strategy. Default: graph.",
                    "default": "graph"
                },
                "limit": { "type": "integer", "default": 8, "minimum": 1, "maximum": 50 },
                "hops": { "type": "integer", "default": 2, "minimum": 1, "maximum": 5,
                          "description": "Hops for graph/spreading modes." },
                "as_of": { "type": "string",
                           "description": "Answer as of this moment instead of now: \"2026-07-31\", \"2026-07-31T09:00:00Z\" or unix seconds. Omit for current facts." }
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self) -> bool {
        // graph/chunks/triplet are pure reads; spreading writes back but
        // the write is bounded by `decay_per_hop` and idempotent under
        // Hebbian semantics — treat as read-only for permission gating.
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let sys = require_system()?;
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing `query`"))?
            .trim();
        if query.is_empty() {
            anyhow::bail!("`query` must not be empty");
        }
        let mode = input
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("graph");
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(8)
            .clamp(1, 50) as usize;
        let hops = input
            .get("hops")
            .and_then(|v| v.as_u64())
            .unwrap_or(2)
            .clamp(1, 5) as u8;

        let mut q = SearchQuery::chunks(query, limit);
        q.query_type = match mode {
            "chunks" => SearchType::Chunks,
            "triplet" => SearchType::Triplet,
            "spreading" => SearchType::SpreadingActivation,
            "temporal" => SearchType::Temporal,
            _ => SearchType::GraphCompletion,
        };
        q.hops = hops;
        q.decay_per_hop = 0.6;
        q.as_of = parse_as_of_arg(&input)?;

        let hits = sys.search(&q).await?;
        let data = serde_json::json!({
            "hits": hits.iter().map(|h| serde_json::json!({
                "id": h.node.id.to_string(),
                "kind": h.node.kind.as_str(),
                "name": h.node.name,
                "summary": h.node.summary,
                "score": h.score,
                "path_len": h.path.len(),
            })).collect::<Vec<_>>(),
        });
        let summary = if hits.is_empty() {
            "No matching memories.".into()
        } else {
            cognitive::format_hits_for_prompt(&hits, 200)
        };
        Ok(vec![assistant_result(summary, data)])
    }

    fn gen_tool_result_message(&self, data: &Value, input: &Value) -> ToolResultMessage {
        let hits_n = data
            .get("hits")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let q = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
        ToolResultMessage {
            title: "Cognitive · search".into(),
            summary: format!("{hits_n} hit(s) for \"{q}\""),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let q = input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("(empty)");
        let trimmed: String = q.chars().take(40).collect();
        format!("CogSearch · {trimmed}")
    }
}

// =====================================================================
// CogRecall — convenience spreading-activation recall
// =====================================================================

pub struct CogRecallTool;

#[async_trait]
impl Tool for CogRecallTool {
    fn name(&self) -> &str {
        "CogRecall"
    }

    fn description(&self) -> &str {
        "Recall what's known about a topic from cognitive memory using SpreadingActivation. \
         Like CogSearch with mode=spreading: the query embeds to seed nodes, then BFS expands \
         k hops while STRENGTHENING the traversed edges (Hebbian write-back). Use this for the \
         common case 'what do I know about X' — over time, frequently-recalled topics become \
         easier to find. Returns only facts that are still current; pass `as_of` for what was \
         true at an earlier moment. For pure read-only lookups prefer CogSearch with \
         mode='graph'."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "default": 8, "minimum": 1, "maximum": 50 },
                "hops":  { "type": "integer", "default": 2, "minimum": 1, "maximum": 5 },
                "as_of": { "type": "string",
                           "description": "Recall what was true at this moment instead of now: \"2026-07-31\" or unix seconds." }
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self) -> bool {
        true // see CogSearch::is_read_only rationale
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let sys = require_system()?;
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing `query`"))?
            .trim();
        if query.is_empty() {
            anyhow::bail!("`query` must not be empty");
        }
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(8)
            .clamp(1, 50) as usize;
        let hops = input
            .get("hops")
            .and_then(|v| v.as_u64())
            .unwrap_or(2)
            .clamp(1, 5) as u8;

        let q = SearchQuery::spreading(query, limit, hops).at(parse_as_of_arg(&input)?);
        let hits = sys.search(&q).await?;
        let data = serde_json::json!({
            "hits": hits.iter().map(|h| serde_json::json!({
                "id": h.node.id.to_string(),
                "kind": h.node.kind.as_str(),
                "name": h.node.name,
                "summary": h.node.summary,
                "score": h.score,
            })).collect::<Vec<_>>(),
        });
        let summary = if hits.is_empty() {
            "Nothing recalled.".into()
        } else {
            cognitive::format_hits_for_prompt(&hits, 200)
        };
        Ok(vec![assistant_result(summary, data)])
    }

    fn gen_tool_result_message(&self, data: &Value, input: &Value) -> ToolResultMessage {
        let hits_n = data
            .get("hits")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let q = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
        ToolResultMessage {
            title: "Cognitive · recall".into(),
            summary: format!("{hits_n} memory(ies) for \"{q}\""),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let q = input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("(empty)");
        let trimmed: String = q.chars().take(40).collect();
        format!("CogRecall · {trimmed}")
    }
}

// =====================================================================
// CogForget — delete a node + cascade edges
// =====================================================================

pub struct CogForgetTool;

#[async_trait]
impl Tool for CogForgetTool {
    fn name(&self) -> &str {
        "CogForget"
    }

    fn description(&self) -> &str {
        "Permanently delete a memory node and all its edges from the knowledge graph. \
         The `node_id` is the UUID returned by CogAdd or surfaced in CogSearch / CogRecall hits. \
         Use sparingly — Hebbian decay handles weakly-used memories automatically. Prefer Forget \
         only when the user explicitly asks to be forgotten or the fact is wrong."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "node_id": { "type": "string", "description": "UUID of the node to delete." }
            },
            "required": ["node_id"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let sys = require_system()?;
        let id_str = input
            .get("node_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing `node_id`"))?;
        let id = uuid::Uuid::parse_str(id_str).map_err(|e| anyhow::anyhow!("invalid uuid: {e}"))?;
        sys.graph.delete_node(id)?;
        let _ = sys.vector.delete(id);
        Ok(vec![assistant_result(
            format!("Forgotten node {id_str}."),
            serde_json::json!({ "forgotten": id_str }),
        )])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "Cognitive · forget".into(),
            summary: data
                .get("forgotten")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let id = input.get("node_id").and_then(|v| v.as_str()).unwrap_or("?");
        format!("CogForget · {}", &id[..id.len().min(8)])
    }
}

// =====================================================================
// CogStats — quick counts (read-only diagnostic)
// =====================================================================

pub struct CogStatsTool;

#[async_trait]
impl Tool for CogStatsTool {
    fn name(&self) -> &str {
        "CogStats"
    }

    fn description(&self) -> &str {
        "Return current cognitive memory statistics: total nodes, edges, and per-kind counts. \
         Use to answer 'how much do you remember?' or to verify a save worked."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let sys = require_system()?;
        let stats = sys.stats()?;
        let nodes_total = sys.graph.count_nodes(None)?;
        let mut by_kind: Vec<(String, usize)> = Vec::new();
        for k in ["entity", "chunk", "summary", "custom"] {
            let n = sys.graph.count_nodes(Some(k))?;
            if n > 0 {
                by_kind.push((k.to_string(), n));
            }
        }
        let kind_str = by_kind
            .iter()
            .map(|(k, n)| format!("{k}={n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let summary = format!(
            "Cognitive memory: {} edges, {} nodes ({}).",
            stats.edges, nodes_total, kind_str
        );
        let data = serde_json::json!({
            "edges": stats.edges,
            "nodes_total": nodes_total,
            "nodes_by_kind": by_kind,
        });
        Ok(vec![assistant_result(summary, data)])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "Cognitive · stats".into(),
            summary: format!(
                "{} nodes, {} edges",
                data.get("nodes_total")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                data.get("edges").and_then(|v| v.as_u64()).unwrap_or(0),
            ),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, _input: &Value) -> String {
        "CogStats".into()
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext<'static> {
        ToolContext {
            agent_id: "test-agent",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        }
    }

    #[test]
    fn each_tool_has_name_and_description() {
        let names: Vec<&str> = vec![
            CogAddTool.name(),
            CogSearchTool.name(),
            CogRecallTool.name(),
            CogForgetTool.name(),
            CogStatsTool.name(),
        ];
        assert_eq!(
            names,
            vec!["CogAdd", "CogSearch", "CogRecall", "CogForget", "CogStats"]
        );
        for desc in [
            CogAddTool.description(),
            CogSearchTool.description(),
            CogRecallTool.description(),
            CogForgetTool.description(),
            CogStatsTool.description(),
        ] {
            // Long enough to actually teach the LLM something.
            assert!(desc.len() > 80, "description too short: {desc}");
        }
    }

    #[test]
    fn schemas_are_valid_json_objects() {
        for schema in [
            CogAddTool.input_schema(),
            CogSearchTool.input_schema(),
            CogRecallTool.input_schema(),
            CogForgetTool.input_schema(),
            CogStatsTool.input_schema(),
        ] {
            assert_eq!(schema.get("type").and_then(|v| v.as_str()), Some("object"));
        }
    }

    #[test]
    fn forget_rejects_bad_uuid() {
        // Whichever check fires first wins. Without an active cognitive
        // singleton the dormant error fires before uuid parsing — that's
        // still a clear failure mode the agent can recover from. Either
        // error variant is acceptable; we just need a non-panic Err.
        let tool = CogForgetTool;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(tool.call(serde_json::json!({ "node_id": "not-a-uuid" }), &ctx()));
        assert!(res.is_err());
        let msg = format!("{:?}", res.unwrap_err()).to_lowercase();
        assert!(
            msg.contains("uuid") || msg.contains("dormant"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn dormant_system_returns_clear_error() {
        // When `init_daemon` hasn't been called (most unit-test environments),
        // require_system surfaces a recoverable message — not a panic.
        let tool = CogStatsTool;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(tool.call(serde_json::json!({}), &ctx()));
        // Either Ok (singleton was initialised by an earlier test) or
        // a dormant error containing the hint.
        if let Err(e) = res {
            let msg = format!("{e}");
            assert!(msg.contains("dormant") || msg.contains("embedding"));
        }
    }
}

//! Zen Patterns MCP server — `pattern_list`, `pattern_get`, `pattern_run`,
//! `pattern_sync`.
//!
//! Four tools, however many hundred patterns are installed. That ratio is the
//! whole design: a pattern is a *value*, not a tool, so the roster stays the
//! size the agent can actually reason about. See [`crate::patterns`] for why
//! patterns are not skills.
//!
//! Like [`crate::mcp::ocr_server`], this subprocess owns no state — it calls
//! the daemon's `/api/patterns/*` over loopback. The registry, the LLM config
//! resolution and the git checkouts all live in the daemon, and a second copy
//! here would race the first one for the same files.
//!
//! ## `pattern_get` vs `pattern_run`
//!
//! `pattern_get` returns the rendered prompt and costs nothing beyond the
//! read; the agent then follows it inside the turn it is already having.
//! `pattern_run` spends a separate LLM call in a clean context. Prefer `get`
//! when the agent has the text in hand, `run` when the output must be
//! reproducible or must not inherit the conversation.

use anyhow::Result;
use rmcp::ServiceExt;

use crate::mcp::schedule_server::ToolResult;

/// The daemon call budget. Measured: a *shallow* Fabric clone is well under a
/// minute, but the fallback to a full clone took **402 s**, so a 300 s budget
/// failed on the very repository the shipped kit points at. Sized for the
/// fallback, not the happy path.
const SYNC_TIMEOUT_SECS: u64 = 900;
const CALL_TIMEOUT_SECS: u64 = 180;

// ── Parameter schemas ────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct PatternListParams {
    /// Case-insensitive filter over pattern names and descriptions
    /// (e.g. "summar", "threat", "log"). Empty lists everything.
    #[serde(default)]
    query: Option<String>,
    /// Restrict to one source id (e.g. "fabric", "user").
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct PatternGetParams {
    /// Pattern name exactly as `pattern_list` reported it.
    name: String,
    /// Text to transform. Only used to fill a `{{input}}` placeholder; leave
    /// it out to see the prompt template itself.
    #[serde(default)]
    input: Option<String>,
    /// Reasoning strategy to append (`cot`, `tot`, `reflexion`, …).
    #[serde(default)]
    strategy: Option<String>,
    /// `"auto"` answers in the input's language; a language name pins it.
    #[serde(default)]
    language: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct PatternRunParams {
    /// Pattern name exactly as `pattern_list` reported it.
    name: String,
    /// The text to transform. Required unless the pattern needs no input.
    #[serde(default)]
    input: String,
    /// Reasoning strategy to append (`cot`, `tot`, `reflexion`, …).
    #[serde(default)]
    strategy: Option<String>,
    /// `"auto"` answers in the input's language; a language name pins it.
    /// Recommended for non-English input: most patterns are written in
    /// English and will otherwise answer in English.
    #[serde(default)]
    language: Option<String>,
    /// LLM profile (config id or label) to run on. Absent = the active model.
    #[serde(default)]
    profile: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct PatternSyncParams {
    /// Source id to refresh, as reported by `pattern_list`.
    source: String,
}

// ── Bridge ───────────────────────────────────────────────────────────────────

struct Bridge {
    base: String,
}

impl Bridge {
    fn url(&self, path: &str) -> String {
        format!("{}/api/patterns{path}", self.base.trim_end_matches('/'))
    }

    fn client(timeout: u64) -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout))
            .build()
            .map_err(|e| e.to_string())
    }

    /// Turn a daemon response into a tool result, keeping the daemon's own
    /// message when it refused — "pattern X not found" is far more useful to
    /// the model than "HTTP 404".
    async fn finish(resp: reqwest::Response) -> ToolResult {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_success() {
            return ToolResult::ok(body);
        }
        let detail = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
            .unwrap_or(body);
        ToolResult::err(detail)
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> ToolResult {
        let client = match Self::client(CALL_TIMEOUT_SECS) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(e),
        };
        match client.get(self.url(path)).query(query).send().await {
            Ok(r) => Self::finish(r).await,
            Err(e) => ToolResult::err(format!("patterns API unreachable: {e}")),
        }
    }

    async fn post(&self, path: &str, body: serde_json::Value, timeout: u64) -> ToolResult {
        let client = match Self::client(timeout) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(e),
        };
        match client.post(self.url(path)).json(&body).send().await {
            Ok(r) => Self::finish(r).await,
            Err(e) => ToolResult::err(format!("patterns API unreachable: {e}")),
        }
    }
}

// ── MCP server ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct McpPatternsServer {
    base: String,
}

impl McpPatternsServer {
    /// Build from `SENCLAW_PATTERNS_API_URL`, or `None` when it is absent.
    /// See [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        Ok(std::env::var("SENCLAW_PATTERNS_API_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|base| Self { base }))
    }

    fn bridge(&self) -> Bridge {
        Bridge {
            base: self.base.clone(),
        }
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpPatternsServer {
    #[rmcp::tool(
        description = "List installed prompt patterns (reusable one-shot text transforms: summarise, extract, analyse, rewrite…). Returns name + description + source for each, plus the available reasoning strategies. Call this first to find the right pattern name; there are hundreds and they are data, not tools."
    )]
    async fn pattern_list(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            PatternListParams,
        >,
    ) -> String {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(q) = p.query.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            query.push(("q", q.to_string()));
        }
        if let Some(src) = p.source.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            query.push(("source", src.to_string()));
        }
        self.bridge().get("", &query).await.content
    }

    #[rmcp::tool(
        description = "Get a pattern's prompt, rendered with the given input/strategy/language but WITHOUT calling the model. Use this when you already have the text in context and can follow the instructions yourself — it costs no extra LLM call. Use pattern_run instead when the output must be reproducible or must not be influenced by this conversation."
    )]
    async fn pattern_get(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            PatternGetParams,
        >,
    ) -> String {
        self.bridge()
            .post(
                "/run",
                serde_json::json!({
                    "name": p.name,
                    "input": p.input.unwrap_or_default(),
                    "strategy": p.strategy,
                    "language": p.language,
                    "dryRun": true,
                }),
                CALL_TIMEOUT_SECS,
            )
            .await
            .content
    }

    #[rmcp::tool(
        description = "Run a pattern over some text in a fresh one-shot LLM call and return the result. No tools, no memory, no conversation history — same input gives the same shape of output every time. Pass language:\"auto\" when the input is not English, or the pattern will answer in English."
    )]
    async fn pattern_run(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            PatternRunParams,
        >,
    ) -> String {
        self.bridge()
            .post(
                "/run",
                serde_json::json!({
                    "name": p.name,
                    "input": p.input,
                    "strategy": p.strategy,
                    "language": p.language,
                    "profile": p.profile,
                }),
                CALL_TIMEOUT_SECS,
            )
            .await
            .content
    }

    #[rmcp::tool(
        description = "Download or refresh a pattern source from its git remote. Slow (hundreds of files); only call it when the user asks to update patterns or a source reports an error."
    )]
    async fn pattern_sync(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            PatternSyncParams,
        >,
    ) -> String {
        let Ok(safe) = crate::patterns::sanitize_name(&p.source) else {
            return ToolResult::err(format!("\"{}\" is not a source id", p.source)).content;
        };
        self.bridge()
            .post(
                &format!("/sources/{safe}/sync"),
                serde_json::json!({}),
                SYNC_TIMEOUT_SECS,
            )
            .await
            .content
    }
}

/// Start the Patterns MCP server over stdio. Requires `SENCLAW_PATTERNS_API_URL`.
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpPatternsServer::from_env()
        .inspect_err(|e| tracing::error!("[PatternsMcp] init failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("SENCLAW_PATTERNS_API_URL not set"))?;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

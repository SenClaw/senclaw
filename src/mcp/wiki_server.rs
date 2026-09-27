//! Local git-backed Wiki MCP server — uses [`crate::wiki::WikiManager`] only.
//!
//! No external HTTP/API. Tools map to file operations under the configured wiki root.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use rmcp::ServiceExt;

use crate::mcp::schedule_server::ToolResult;
use crate::wiki::manager::WikiManager;

// ===== Server logic =====

struct WikiMcpCore {
    wiki: WikiManager,
}

impl WikiMcpCore {
    fn new(wiki_dir: PathBuf) -> Self {
        Self {
            wiki: WikiManager::new(wiki_dir),
        }
    }

    fn wiki_status(&self) -> ToolResult {
        let root = self.wiki.wiki_dir.display().to_string();
        match self.wiki.get_stats() {
            Ok(stats) => {
                let pretty = serde_json::to_string_pretty(&stats).unwrap_or_default();
                ToolResult::ok(format!("wiki_root: {root}\n{pretty}"))
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }

    fn wiki_tree(&self) -> ToolResult {
        match self.wiki.tree_text() {
            Ok(text) => ToolResult::ok(text),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }

    fn wiki_read(&self, path: &str) -> ToolResult {
        match self.wiki.read_file(path) {
            Ok(doc) => {
                let v = serde_json::json!({
                    "path": doc.path,
                    "frontmatter": doc.frontmatter,
                    "content": doc.content,
                    "git_log": doc.git_log,
                });
                ToolResult::ok(serde_json::to_string_pretty(&v).unwrap_or_default())
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }

    async fn wiki_write(
        &self,
        path: &str,
        content: &str,
        tags: Option<Vec<String>>,
        commit_message: Option<&str>,
        source: Option<&str>,
    ) -> ToolResult {
        let tags_ref = tags.as_deref();
        match self
            .wiki
            .write_file(path, content, source, tags_ref, commit_message)
            .await
        {
            Ok(()) => ToolResult::ok(format!("Written: {path}")),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }

    fn wiki_search(
        &self,
        query: &str,
        filter_tags: Option<Vec<String>>,
        limit: Option<usize>,
    ) -> ToolResult {
        let ft = filter_tags.as_deref();
        match self.wiki.search(query, ft, limit) {
            Ok(rows) => {
                let pretty = serde_json::to_string_pretty(&rows).unwrap_or_default();
                ToolResult::ok(pretty)
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }

    fn wiki_stats(&self) -> ToolResult {
        match self.wiki.get_stats() {
            Ok(s) => ToolResult::ok(serde_json::to_string_pretty(&s).unwrap_or_default()),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }

    async fn wiki_mkdir(&self, path: &str) -> ToolResult {
        match self.wiki.mkdir(path).await {
            Ok(()) => ToolResult::ok(format!("Created directory: {path}")),
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

// ===== MCP params =====

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct WikiReadParams {
    /// Relative path under wiki root (e.g. `inbox/note.md`)
    path: String,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct WikiWriteParams {
    path: String,
    content: String,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    #[serde(rename = "commitMessage")]
    commit_message: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct WikiSearchParams {
    query: String,
    #[serde(default)]
    #[serde(rename = "filterTags")]
    filter_tags: Option<Vec<String>>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct WikiMkdirParams {
    path: String,
}

// ===== MCP stdio server =====

#[derive(Clone)]
pub struct McpWikiServer {
    inner: Arc<WikiMcpCore>,
}

impl McpWikiServer {
    fn inner(&self) -> &WikiMcpCore {
        &self.inner
    }

    /// Build from `SENCLAW_WIKI_DIR`, or `None` when that var is absent.
    ///
    /// `None` is how the aggregated `senclaw-core` server learns this child
    /// was not configured for the current agent — it is not an error.
    pub async fn from_env() -> anyhow::Result<Option<Self>> {
        let Ok(wiki_dir) = std::env::var("SENCLAW_WIKI_DIR") else {
            return Ok(None);
        };
        let core = WikiMcpCore::new(PathBuf::from(&wiki_dir));
        core.wiki.ensure_init().await?;
        Ok(Some(Self {
            inner: Arc::new(core),
        }))
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpWikiServer {
    #[rmcp::tool(description = "Show wiki root path and summary statistics (git-backed)")]
    fn wiki_status(&self) -> String {
        self.inner().wiki_status().content
    }

    #[rmcp::tool(description = "List the wiki directory tree as plain text")]
    fn wiki_tree(&self) -> String {
        self.inner().wiki_tree().content
    }

    #[rmcp::tool(description = "Read a markdown wiki page by relative path")]
    fn wiki_read(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            WikiReadParams,
        >,
    ) -> String {
        self.inner().wiki_read(&p.path).content
    }

    #[rmcp::tool(
        description = "Create or update a markdown wiki page (auto frontmatter + git commit)"
    )]
    async fn wiki_write(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            WikiWriteParams,
        >,
    ) -> String {
        self.inner()
            .wiki_write(
                &p.path,
                &p.content,
                p.tags,
                p.commit_message.as_deref(),
                p.source.as_deref(),
            )
            .await
            .content
    }

    #[rmcp::tool(description = "Search wiki pages by title, filename, or tags")]
    fn wiki_search(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            WikiSearchParams,
        >,
    ) -> String {
        self.inner()
            .wiki_search(&p.query, p.filter_tags, p.limit)
            .content
    }

    #[rmcp::tool(description = "Detailed wiki stats (categories, tags, recent files)")]
    fn wiki_stats(&self) -> String {
        self.inner().wiki_stats().content
    }

    #[rmcp::tool(description = "Create a subdirectory under the wiki (tracked in git)")]
    async fn wiki_mkdir(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            WikiMkdirParams,
        >,
    ) -> String {
        self.inner().wiki_mkdir(&p.path).await.content
    }
}

/// Start the Wiki MCP server over stdio. Requires `SENCLAW_WIKI_DIR`.
pub async fn run_stdio_server() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpWikiServer::from_env()
        .await
        .inspect_err(|e| tracing::error!("[WikiMcp] init failed: {e}"))?
        .context("SENCLAW_WIKI_DIR not set")?;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

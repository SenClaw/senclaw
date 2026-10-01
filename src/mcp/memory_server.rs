//! Memory MCP server. Port target: src-old/mcp/memory-server.ts
//!
//! Tools: memory_search, memory_get.
//! Provides read-only memory retrieval via FTS5 + vector hybrid search.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::db::Db;
use crate::mcp::schedule_server::ToolResult;

use crate::memory::curated;
use crate::memory::embedding::{create_embedding_provider, EmbeddingProvider};
use crate::memory::fts_search::{self, SearchOptions};
use rmcp::ServiceExt;

// ===== MCP stdio server =====

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct MemorySearchParams {
    query: String,
    #[serde(default)]
    #[serde(rename = "maxResults")]
    max_results: Option<usize>,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct MemoryGetParams {
    #[serde(rename = "relPath")]
    rel_path: String,
    #[serde(default)]
    #[serde(rename = "startLine")]
    start_line: Option<u32>,
    #[serde(default)]
    #[serde(rename = "endLine")]
    end_line: Option<u32>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct MemorySaveParams {
    /// Kebab-case slug; becomes the filename. Free text is normalized automatically.
    name: String,
    /// Short recall hook (≤120 chars) — this is what recall matches on.
    description: String,
    /// Markdown body. For type=project|feedback use **Why:** / **How to apply:** sections.
    body: String,
    /// project | reference | feedback | user. Defaults to "project".
    #[serde(default)]
    #[serde(rename = "type")]
    mem_type: Option<String>,
    /// Optional human-readable title for the index (defaults to the de-slugified name).
    #[serde(default)]
    title: Option<String>,
    /// Overwrite an existing same-name memory (update-not-duplicate). Defaults to false.
    #[serde(default)]
    supersede: Option<bool>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct MemoryRecallParams {
    query: String,
    #[serde(default)]
    #[serde(rename = "maxResults")]
    max_results: Option<usize>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct MemoryDeleteParams {
    name: String,
}

#[derive(Clone)]
pub struct McpMemoryServer {
    db_path: String,
    folder: String,
    profiles_dir: PathBuf,
    custom_memory_dir: Option<PathBuf>,
}

impl McpMemoryServer {
    /// Build from the memory env trio, or `None` when any is absent. See
    /// [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        let (Ok(db_path), Ok(folder), Ok(profiles_dir)) = (
            std::env::var("SENCLAW_DB_PATH"),
            std::env::var("SENCLAW_FOLDER"),
            std::env::var("SENCLAW_PROFILES_DIR"),
        ) else {
            return Ok(None);
        };
        Ok(Some(Self {
            db_path,
            folder,
            profiles_dir: PathBuf::from(profiles_dir),
            custom_memory_dir: std::env::var("SENCLAW_CUSTOM_MEMORY_DIR")
                .ok()
                .map(PathBuf::from),
        }))
    }

    /// The per-folder base dir (mirrors `MemoryManager::get_memory_dir_for_folder`):
    /// custom cowork dir, else `profiles_dir/{folder}`. `MEMORY.md` lives here; curated
    /// files live under `<base>/memory/`.
    fn base_dir(&self) -> PathBuf {
        self.custom_memory_dir
            .clone()
            .unwrap_or_else(|| self.profiles_dir.join(&self.folder))
    }

    fn open_db_and_provider(&self) -> Result<(Db, Option<Box<dyn EmbeddingProvider>>)> {
        let cfg = crate::config::Config::from_env();
        let mut db_cfg = cfg.clone();
        db_cfg.paths.db_path = PathBuf::from(&self.db_path);
        let db = Db::open(&db_cfg).context("open memory DB")?;
        let provider = create_embedding_provider(&cfg, Arc::new(Db::open(&db_cfg)?));
        Ok((db, provider))
    }

}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpMemoryServer {
    #[rmcp::tool(description = "Search memories using hybrid FTS5 + vector search")]
    async fn memory_search(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            MemorySearchParams,
        >,
    ) -> String {
        let (db, provider) = match self.open_db_and_provider() {
            Ok(v) => v,
            Err(e) => return format!("Error: {e}"),
        };
        let srv = MemoryServer::new(
            db,
            &self.folder,
            &self.profiles_dir,
            provider,
            self.custom_memory_dir.clone(),
        );
        srv.memory_search(&p.query, p.max_results, p.source.as_deref())
            .await
            .content
    }

    #[rmcp::tool(description = "Retrieve a specific memory file by path and line range")]
    fn memory_get(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            MemoryGetParams,
        >,
    ) -> String {
        let db = match self.open_db_and_provider() {
            Ok((db, _)) => db,
            Err(e) => return format!("Error: {e}"),
        };
        let srv = MemoryServer::new(
            db,
            &self.folder,
            &self.profiles_dir,
            None,
            self.custom_memory_dir.clone(),
        );
        srv.memory_get(&p.rel_path, p.start_line, p.end_line)
            .content
    }

    #[rmcp::tool(
        description = "Save a curated, human-readable memory (frontmatter + body) and update the MEMORY.md index. Use for durable notes: decisions, gotchas, research findings, user requests. Do NOT save facts derivable from code/git/CLAUDE.md. Pass supersede=true to update an existing memory instead of duplicating it."
    )]
    fn memory_save(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            MemorySaveParams,
        >,
    ) -> String {
        let base = self.base_dir();
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mem_type = p.mem_type.as_deref().unwrap_or("project");
        match curated::save(
            &base,
            &p.name,
            &p.description,
            &p.body,
            mem_type,
            p.title.as_deref(),
            &self.folder,
            &date,
            p.supersede.unwrap_or(false),
        ) {
            Ok(s) => format!(
                "{} memory '{}' ({}).",
                if s.updated { "Updated" } else { "Saved" },
                s.name,
                s.path.display()
            ),
            Err(e) => format!("Error: {e}"),
        }
    }

    #[rmcp::tool(
        description = "Recall curated memories relevant to a query (hybrid FTS5 + vector search over saved memories, excluding daily logs). Returns each memory's name, type, hook, and matched snippet."
    )]
    async fn memory_recall(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            MemoryRecallParams,
        >,
    ) -> String {
        let (db, provider) = match self.open_db_and_provider() {
            Ok(v) => v,
            Err(e) => return format!("Error: {e}"),
        };
        let srv = MemoryServer::new(
            db,
            &self.folder,
            &self.profiles_dir,
            provider,
            self.custom_memory_dir.clone(),
        );
        srv.memory_recall(&p.query, p.max_results).await
    }

    #[rmcp::tool(
        description = "Delete a curated memory by name and remove its MEMORY.md index entry. Use to prune memories that turned out to be wrong."
    )]
    fn memory_delete(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            MemoryDeleteParams,
        >,
    ) -> String {
        let base = self.base_dir();
        match curated::delete(&base, &p.name) {
            Ok(true) => format!("Deleted memory '{}'.", curated::slugify(&p.name)),
            Ok(false) => format!("No memory named '{}' found.", curated::slugify(&p.name)),
            Err(e) => format!("Error: {e}"),
        }
    }
}

/// Start the memory MCP server over stdio.
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpMemoryServer::from_env()?
        .context("SENCLAW_DB_PATH / SENCLAW_FOLDER / SENCLAW_PROFILES_DIR not set")?;

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

pub struct MemoryServer {
    db: Db,
    folder: String,
    profiles_dir: PathBuf,
    embedding_provider: Option<Box<dyn EmbeddingProvider>>,
    custom_memory_dir: Option<PathBuf>,
}

impl MemoryServer {
    pub fn new(
        db: Db,
        folder: &str,
        profiles_dir: &Path,
        embedding_provider: Option<Box<dyn EmbeddingProvider>>,
        custom_memory_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            db,
            folder: folder.to_owned(),
            profiles_dir: profiles_dir.to_path_buf(),
            embedding_provider,
            custom_memory_dir,
        }
    }

    fn get_memory_dir(&self) -> &Path {
        self.custom_memory_dir
            .as_ref()
            .map(|p| p.as_path())
            .unwrap_or_else(|| &self.profiles_dir)
    }

    // ===== memory_search =====

    pub async fn memory_search(
        &self,
        query: &str,
        max_results: Option<usize>,
        source: Option<&str>,
    ) -> ToolResult {
        let limit = max_results.unwrap_or(6);
        let opts = SearchOptions {
            max_results: limit + 3,
            min_score: 0.25,
            source: source.map(|s| s.to_owned()),
        };

        let provider_ref: Option<&dyn EmbeddingProvider> = self.embedding_provider.as_deref();

        let raw_results = match fts_search::hybrid_search(
            &self.db,
            &self.folder,
            query,
            provider_ref,
            opts,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("Search error: {e}")),
        };

        // Exclude today's log file (actively written, unstable content)
        let today_file = format!("{}.md", chrono::Utc::now().format("%Y-%m-%d"));
        let results: Vec<_> = raw_results
            .into_iter()
            .filter(|r| !r.path.ends_with(&today_file))
            .take(limit)
            .collect();

        if results.is_empty() {
            return ToolResult::ok("No matching memories found.".into());
        }

        let mut out = format!("Found {} results:\n\n", results.len());
        for (i, r) in results.iter().enumerate() {
            let path_parts: Vec<&str> = r.path.split(&['/', '\\'][..]).collect();
            let display_path = if path_parts.len() >= 2 {
                format!(
                    "{}/{}",
                    path_parts[path_parts.len() - 2],
                    path_parts[path_parts.len() - 1]
                )
            } else {
                r.path.clone()
            };
            out.push_str(&format!(
                "[{}] {}:{}-{} (score: {:.2})\n",
                i + 1,
                display_path,
                r.start_line,
                r.end_line,
                r.score
            ));
            let summary = if r.text.len() > 300 {
                format!(
                    "{}...",
                    crate::util::text::truncate_on_char_boundary(&r.text, 300)
                )
            } else {
                r.text.clone()
            };
            out.push_str(&format!("{summary}\n\n"));
        }

        ToolResult::ok(out.trim().to_string())
    }

    // ===== memory_recall =====

    /// Curated recall: hybrid search scoped to saved memories, deduped per file, each
    /// presented as `name (type) — hook` + the matched snippet.
    pub async fn memory_recall(&self, query: &str, max_results: Option<usize>) -> String {
        let limit = max_results.unwrap_or(5);
        let opts = SearchOptions {
            max_results: limit + 5,
            min_score: 0.25,
            source: Some("memory".to_owned()),
        };
        let provider_ref: Option<&dyn EmbeddingProvider> = self.embedding_provider.as_deref();

        let raw = match fts_search::hybrid_search(&self.db, &self.folder, query, provider_ref, opts)
            .await
        {
            Ok(r) => r,
            Err(e) => return format!("Recall error: {e}"),
        };

        let today_file = format!("{}.md", chrono::Utc::now().format("%Y-%m-%d"));
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out = String::new();
        let mut n = 0usize;
        for r in raw {
            // Skip daily logs and the index itself; curated memories only.
            if r.path.ends_with(&today_file) || r.path.ends_with("MEMORY.md") {
                continue;
            }
            if !seen.insert(r.path.clone()) {
                continue;
            }
            n += 1;
            match curated::read_meta(Path::new(&r.path)) {
                Some(m) => out.push_str(&format!(
                    "[{n}] {} ({}) — {}\n",
                    m.name,
                    if m.mem_type.is_empty() {
                        "memory"
                    } else {
                        &m.mem_type
                    },
                    m.description
                )),
                None => out.push_str(&format!("[{n}] {}\n", r.path)),
            }
            out.push_str(&format!("{}\n\n", truncate_chars(&r.text, 300)));
            if n >= limit {
                break;
            }
        }

        if n == 0 {
            return "No matching memories found.".to_string();
        }
        format!("Recalled {n} memories:\n\n{}", out.trim())
    }

    // ===== memory_get =====

    pub fn memory_get(
        &self,
        rel_path: &str,
        start_line: Option<u32>,
        end_line: Option<u32>,
    ) -> ToolResult {
        let memory_dir = self.get_memory_dir();
        let abs_path = match resolve_memory_path(memory_dir, &self.folder, rel_path) {
            Some(p) => p,
            None => {
                return ToolResult::err(format!(
                    "File not found (path traversal blocked): {rel_path}"
                ))
            }
        };

        if !abs_path.exists() {
            return ToolResult::err(format!("File not found: {rel_path}"));
        }

        let content = match fs::read_to_string(&abs_path) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("Error reading file: {e}")),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len() as u32;

        let start = (start_line.unwrap_or(1)).saturating_sub(1).min(total_lines);
        let end = end_line.unwrap_or(total_lines).min(total_lines).max(start);

        let slice = &lines[start as usize..end as usize];
        let header = format!(
            "{} (lines {}-{} of {}):\n\n",
            rel_path,
            start + 1,
            end,
            total_lines
        );

        ToolResult::ok(format!("{header}{}", slice.join("\n")))
    }
}

/// Truncate to at most `max` chars (not bytes), appending `...` when clipped. Safe for
/// UTF-8 memory bodies (Vietnamese/CJK) unlike a raw byte slice.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}...")
    }
}

/// Resolve a relative memory path to an absolute path, with path-traversal protection.
fn resolve_memory_path(profiles_dir: &Path, folder: &str, relative_path: &str) -> Option<PathBuf> {
    let agent_dir = profiles_dir.join(folder);

    let safe_check =
        |p: &PathBuf| -> bool { p.starts_with(&agent_dir) || p.as_path() == agent_dir.as_path() };

    // Try direct join
    let c1 = agent_dir.join(relative_path);
    if safe_check(&c1) && c1.exists() {
        return Some(c1);
    }

    // Try memory/ subdirectory
    let c2 = agent_dir.join("memory").join(relative_path);
    if safe_check(&c2) && c2.exists() {
        return Some(c2);
    }

    // Return safe path even if file doesn't exist (caller decides handling)
    if safe_check(&c1) {
        return Some(c1);
    }

    None
}

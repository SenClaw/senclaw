//! Litho ([deepwiki-rs](https://github.com/sopaco/deepwiki-rs)) MCP server — wraps the
//! `deepwiki-rs` CLI for C4-style architecture documentation generation.
//!
//! Requires the `deepwiki-rs` binary on `PATH` or set `SENCLAW_LITHO_BINARY` to a full path.
//! LLM credentials can be forwarded via `SENCLAW_LITHO_LLMAPI_BASE_URL` / `SENCLAW_LITHO_LLMAPI_KEY`
//! (usually injected by [`crate::mcp::helper::litho_mcp_config`] from SenClaw config).

use std::path::{Path, PathBuf};

use anyhow::Context;
use rmcp::ServiceExt;

use crate::mcp::schedule_server::ToolResult;

const READ_MAX_BYTES: usize = 512 * 1024;
const OUTPUT_PREVIEW_MAX: usize = 120_000;

fn litho_binary() -> String {
    std::env::var("SENCLAW_LITHO_BINARY").unwrap_or_else(|_| "deepwiki-rs".to_string())
}

fn append_llm_env(cmd: &mut tokio::process::Command) {
    if let Ok(u) = std::env::var("SENCLAW_LITHO_LLMAPI_BASE_URL") {
        if !u.is_empty() {
            cmd.arg("--llm-api-base-url").arg(u);
        }
    }
    if let Ok(k) = std::env::var("SENCLAW_LITHO_LLMAPI_KEY") {
        if !k.is_empty() {
            cmd.arg("--llm-api-key").arg(k);
        }
    }
    if let Ok(m) = std::env::var("SENCLAW_LITHO_MODEL_EFFICIENT") {
        if !m.is_empty() {
            cmd.arg("--model-efficient").arg(m);
        }
    }
}

fn truncate_preview(mut s: String) -> String {
    if s.len() > OUTPUT_PREVIEW_MAX {
        s.truncate(OUTPUT_PREVIEW_MAX);
        s.push_str("\n… [truncated]");
    }
    s
}

async fn run_command(mut cmd: tokio::process::Command, label: &str) -> ToolResult {
    match cmd.output().await {
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            let mut msg =
                format!("{label}\nexit_code={code}\n--- stderr ---\n{stderr}\n--- stdout ---\n");
            msg.push_str(&truncate_preview(stdout));
            if out.status.success() {
                ToolResult::ok(msg)
            } else {
                ToolResult::err(msg)
            }
        }
        Err(e) => ToolResult::err(format!("{label}: failed to run deepwiki-rs: {e}")),
    }
}

fn safe_read_under_output_base(base: &Path, relative_path: &str) -> anyhow::Result<String> {
    let base = base
        .canonicalize()
        .with_context(|| format!("output_dir not found: {}", base.display()))?;
    let rel = PathBuf::from(relative_path);
    if rel.is_absolute() || relative_path.contains("..") {
        anyhow::bail!("relative_path must be relative and must not contain '..'");
    }
    let full = base.join(rel);
    let full = full
        .canonicalize()
        .with_context(|| format!("file not found: {}", full.display()))?;
    if !full.starts_with(&base) {
        anyhow::bail!("path escapes output_dir");
    }
    let meta = std::fs::metadata(&full)?;
    if meta.len() as usize > READ_MAX_BYTES {
        anyhow::bail!("file too large (max {READ_MAX_BYTES} bytes)");
    }
    std::fs::read_to_string(&full).with_context(|| format!("read {}", full.display()))
}

// ===== MCP params =====

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct LithoGenerateParams {
    /// Path to the repository or source tree to analyze (passed to `deepwiki-rs -p`).
    project_path: String,
    /// Output directory for generated docs (passed to `-o`, default `./litho.docs` in Litho).
    #[serde(default)]
    output_path: Option<String>,
    /// Optional `litho.toml` path (`-c`).
    #[serde(default)]
    config_path: Option<String>,
    /// Target language code: en, zh, ja, vi, …
    #[serde(default)]
    target_language: Option<String>,
    #[serde(default)]
    skip_preprocessing: bool,
    #[serde(default)]
    skip_research: bool,
    #[serde(default)]
    skip_documentation: bool,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct LithoSyncKnowledgeParams {
    /// Working directory where `litho.toml` is expected (subprocess `current_dir`).
    working_directory: String,
    #[serde(default)]
    config_path: Option<String>,
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct LithoReadDocParams {
    /// Directory that Litho wrote docs to (same as `-o` for generation).
    output_dir: String,
    /// Path relative to `output_dir`, e.g. `1. Project Overview.md`.
    relative_path: String,
}

// ===== MCP stdio server =====

#[derive(Clone)]
pub struct McpLithoServer {}

impl McpLithoServer {
    /// Always available — every setting is read per call from
    /// `SENCLAW_LITHO_*`, so there is nothing to capture at construction.
    /// Returns `Option` only to match the other children.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        Ok(Some(Self {}))
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpLithoServer {
    #[rmcp::tool(
        description = "Run Litho (deepwiki-rs) to generate C4 architecture markdown docs from a codebase. May take several minutes; ensure MCP timeout is large enough."
    )]
    async fn litho_generate(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            LithoGenerateParams,
        >,
    ) -> String {
        let bin = litho_binary();
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.arg("-p").arg(&p.project_path);
        if let Some(ref o) = p.output_path {
            cmd.arg("-o").arg(o);
        }
        if let Some(ref c) = p.config_path {
            cmd.arg("-c").arg(c);
        }
        if let Some(ref lang) = p.target_language {
            cmd.arg("--target-language").arg(lang);
        }
        if p.skip_preprocessing {
            cmd.arg("--skip-preprocessing");
        }
        if p.skip_research {
            cmd.arg("--skip-research");
        }
        if p.skip_documentation {
            cmd.arg("--skip-documentation");
        }
        append_llm_env(&mut cmd);
        run_command(cmd, "litho_generate").await.content
    }

    #[rmcp::tool(
        description = "Sync external knowledge sources into Litho's cache (`deepwiki-rs sync-knowledge`). Run from the directory containing litho.toml."
    )]
    async fn litho_sync_knowledge(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            LithoSyncKnowledgeParams,
        >,
    ) -> String {
        let bin = litho_binary();
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.arg("sync-knowledge");
        if p.force {
            cmd.arg("--force");
        }
        if let Some(ref c) = p.config_path {
            cmd.arg("-c").arg(c);
        }
        cmd.current_dir(&p.working_directory);
        run_command(cmd, "litho_sync_knowledge").await.content
    }

    #[rmcp::tool(
        description = "Read a generated markdown file from a Litho output directory (safe path join)."
    )]
    fn litho_read_doc(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            LithoReadDocParams,
        >,
    ) -> String {
        let base = PathBuf::from(&p.output_dir);
        match safe_read_under_output_base(&base, &p.relative_path) {
            Ok(text) => ToolResult::ok(text).content,
            Err(e) => ToolResult::err(e.to_string()).content,
        }
    }
}

/// Start the Litho MCP server over stdio.
pub async fn run_stdio_server() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpLithoServer {};
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

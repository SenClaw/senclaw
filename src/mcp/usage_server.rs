//! Usage MCP server — lets the agent answer "hôm nay tốn bao nhiêu token/tiền"
//! from the accounting tables (`llm_usage_log` / `llm_usage_daily` /
//! `model_pricing`) without the user opening the dashboard.
//!
//! Naming per CLAUDE.md: server `senclaw-usage`, tool prefix `usage_`.
//! Read-only — pricing edits go through the REST API/UI, not the agent.

use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::ServiceExt;

use crate::db::usage::BREAKDOWN_KEYS;
use crate::db::Db;

// ───────────────────────── param structs ─────────────────────────

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct BreakdownParams {
    /// Group dimension: "model" (default), "source" (agent/subagent/compact/
    /// hook/bridge/cognitive/embedding/app_direct), "jid" (chat/group), or
    /// "app" (Space App id).
    #[serde(default)]
    by: Option<String>,
    /// Window in days (default 7, max 90 — raw-log retention).
    #[serde(default)]
    days: Option<u32>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct QueryParams {
    /// Max raw rows to return, newest first (default 50, max 200).
    #[serde(default)]
    limit: Option<u32>,
    /// Only rows from this source (e.g. "agent", "bridge", "cognitive").
    #[serde(default)]
    source: Option<String>,
    /// Only rows whose model id contains this substring.
    #[serde(default)]
    model: Option<String>,
}

// ───────────────────────── MCP server ─────────────────────────

#[derive(Clone)]
pub struct McpUsageServer {
    db: Arc<Db>,
}

impl McpUsageServer {
    /// Build from `SENCLAW_DB_PATH`, or `None` when it is absent. See
    /// [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        Ok(crate::mcp::helper::shared_env_db()?.map(|db| Self { db }))
    }

    fn overview_impl(&self) -> Result<String> {
        let now = chrono::Utc::now();
        let until = now.timestamp_millis() + 60_000;
        let today_start = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc()
            .timestamp_millis();
        let week_start = (now - chrono::Duration::days(7)).timestamp_millis();
        let month_start = (now - chrono::Duration::days(30)).timestamp_millis();
        let today = self.db.usage_totals(today_start, until)?;
        let week = self.db.usage_totals(week_start, until)?;
        let month = self.db.usage_totals(month_start, until)?;
        Ok(serde_json::json!({
            "today": today, "last7d": week, "last30d": month,
            "note": "estCostUsd chỉ tính phần token có giá trong model_pricing; \
                     unpricedTokens là phần chưa có giá (không phải $0).",
        })
        .to_string())
    }

    fn breakdown_impl(&self, p: BreakdownParams) -> Result<String> {
        let by_raw = p.by.unwrap_or_else(|| "model".into());
        let by = if by_raw == "app" {
            "app_id"
        } else {
            by_raw.as_str()
        };
        anyhow::ensure!(
            BREAKDOWN_KEYS.contains(&by),
            "by phải là model|source|jid|app"
        );
        let days = p.days.unwrap_or(7).clamp(1, 90);
        let now = chrono::Utc::now().timestamp_millis();
        let rows = self
            .db
            .usage_breakdown(by, now - days as i64 * 86_400_000, now + 60_000)?;
        Ok(serde_json::json!({ "by": by_raw, "days": days, "rows": rows }).to_string())
    }

    fn query_impl(&self, p: QueryParams) -> Result<String> {
        let limit = p.limit.unwrap_or(50).clamp(1, 200);
        // Over-fetch then filter in-process: the raw table is indexed by id,
        // and post-filtering 5× the ask is cheaper than growing the DB API for
        // an agent-side debug tool.
        let rows = self.db.usage_log_recent(limit * 5, None)?;
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|r| p.source.as_deref().is_none_or(|s| r.source == s))
            .filter(|r| p.model.as_deref().is_none_or(|m| r.model.contains(m)))
            .take(limit as usize)
            .collect();
        Ok(serde_json::json!({ "rows": rows }).to_string())
    }
}

fn err_json(e: anyhow::Error) -> String {
    serde_json::json!({ "error": e.to_string() }).to_string()
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpUsageServer {
    #[rmcp::tool(
        description = "Tổng token in/out + chi phí ước tính (USD) hôm nay / 7 ngày / 30 ngày, \
                       gộp mọi nguồn (agent, subagent, compact, bridge app, cognitive, embedding)."
    )]
    fn usage_overview(&self) -> String {
        self.overview_impl().unwrap_or_else(err_json)
    }

    #[rmcp::tool(
        description = "Phân rã token/chi phí theo một chiều: by=model|source|jid|app, \
                       trong N ngày gần nhất (mặc định 7). Trả rows đã sắp theo tổng token giảm dần."
    )]
    fn usage_breakdown(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            BreakdownParams,
        >,
    ) -> String {
        self.breakdown_impl(p).unwrap_or_else(err_json)
    }

    #[rmcp::tool(
        description = "Các LLM call gần nhất (raw log, mới nhất trước) — debug xem call nào \
                       đốt token. Lọc được theo source và model (substring)."
    )]
    fn usage_query(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            QueryParams,
        >,
    ) -> String {
        self.query_impl(p).unwrap_or_else(err_json)
    }
}

/// Start the usage MCP server over stdio. Reads config from the env set by
/// [`crate::mcp::helper::usage_mcp_config`].
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpUsageServer::from_env()?.context("SENCLAW_DB_PATH not set")?;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

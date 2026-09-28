//! `senclaw-core` — every built-in MCP server hosted by a single process.
//!
//! Historically `AgentPool` spawned one subprocess per built-in server, so a
//! chat with wiki + workspace + memory + browser + … cost fourteen `senclaw`
//! processes, fourteen stdio pipes and fourteen MCP handshakes before the first
//! tool call. This server hosts the same children in-process and republishes
//! their tools under one flat namespace, so the pool spawns one.
//!
//! Nothing about the tools themselves changes: each child is the exact struct
//! its own `*-server` subcommand runs, built from the exact same env vars, and
//! a call is handed to that child's generated [`ToolRouter`] untouched. The
//! per-server subcommands still work — this is an additional way to run them,
//! not a replacement.
//!
//! A child whose env is absent is simply skipped (see
//! [`crate::mcp::wiki_server::McpWikiServer::from_env`]), which is what makes
//! one config usable for every kind of agent: pass the env a chat needs and it
//! gets those tools.

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServiceExt};

use super::background_server::McpBackgroundServer;
use super::browser_agent_server::McpBrowserAgentServer;
use super::browser_server::McpBrowserServer;
use super::dispatch_server::McpDispatchServer;
use super::js_server::McpJsServer;
use super::litho_server::McpLithoServer;
use super::memory_server::McpMemoryServer;
use super::ocr_server::McpOcrServer;
use super::patterns_server::McpPatternsServer;
use super::sandbox_server::McpSandboxServer;
use super::user_profile_server::McpUserProfileServer;
use super::schedule_server::McpScheduleServer;
use super::send_server::McpSendServer;
use super::space_server::McpSpaceServer;
use super::usage_server::McpUsageServer;
use super::virtual_server::McpVirtualServer;
use super::wiki_server::McpWikiServer;
use super::docs_server::McpDocsServer;
use super::workspace_server::McpWorkspaceServer;

/// The aggregated server's own tool — lets a client ask which children are
/// live without inferring it from tool names.
pub const STATUS_TOOL: &str = "core_status";

/// What this server calls itself in the MCP handshake.
///
/// Must be set explicitly: `ServerInfo::new` defaults to
/// `Implementation::from_build_env()`, which reports the *rmcp crate* name, so
/// every SenClaw MCP server would introduce itself as "rmcp". A client that
/// keys on `serverInfo.name` — the app's MCP screen among them — needs the
/// name it sees here to match the name it configured.
/// The user-facing name. The module, its subcommand (`core-server`) and the
/// `SENCLAW_CORE_SERVERS` env var keep the old spelling — only what people
/// read was renamed.
pub const SERVER_NAME: &str = "senclaw-core";

/// A live child: the server struct plus the router `#[tool_router]` generated
/// for it. The router is built once instead of per call — `tool_router()`
/// rebuilds the whole route map, which is wasteful on a hot path.
struct Child<S> {
    server: S,
    router: ToolRouter<S>,
    label: &'static str,
}

/// Run `$body` once per child that was successfully built, binding `$child`.
///
/// The children have different types, so this cannot be a loop over a `Vec` —
/// each arm is monomorphised separately. Order is stable and defines which
/// child wins a duplicate tool name.
macro_rules! for_each_child {
    ($self:ident, $child:ident => $body:block) => {
        if let Some($child) = &$self.wiki $body
        if let Some($child) = &$self.workspace $body
        if let Some($child) = &$self.docs $body
        if let Some($child) = &$self.memory $body
        if let Some($child) = &$self.schedule $body
        if let Some($child) = &$self.background $body
        if let Some($child) = &$self.usage $body
        if let Some($child) = &$self.dispatch $body
        if let Some($child) = &$self.virtual_agents $body
        if let Some($child) = &$self.space $body
        if let Some($child) = &$self.send $body
        if let Some($child) = &$self.browser $body
        if let Some($child) = &$self.browser_agent $body
        if let Some($child) = &$self.ocr $body
        if let Some($child) = &$self.patterns $body
        if let Some($child) = &$self.litho $body
        if let Some($child) = &$self.js $body
        if let Some($child) = &$self.sandbox $body
        if let Some($child) = &$self.user_profile $body
    };
}

/// Turn a `from_env()` result into a [`Child`], logging why it was skipped.
///
/// A child that fails to build must not take the others down with it: losing
/// the browser tools because the extension is absent is survivable, losing the
/// whole tool set over it is not.
///
/// `$wanted` is the caller's allowlist. Env presence alone cannot decide
/// membership, because the children share variables — configuring dispatch
/// also happens to satisfy the virtual-agent server, which would then appear
/// in chats that never had it.
macro_rules! build_child {
    ($wanted:expr, $label:literal, $ty:ty, $built:expr) => {
        if !$wanted.wants($label) {
            None
        } else {
            match $built {
                Ok(Some(server)) => Some(Child {
                    server,
                    router: <$ty>::tool_router(),
                    label: $label,
                }),
                Ok(None) => {
                    tracing::debug!(
                        "[Core] {} not configured for this agent — skipped",
                        $label
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!("[Core] {} unavailable: {e}", $label);
                    None
                }
            }
        }
    };
}

/// The `SENCLAW_CORE_SERVERS` allowlist. Unset means "every child whose env
/// is present", which is what a hand-run `senclaw core-server` wants.
struct Wanted(Option<Vec<String>>);

impl Wanted {
    fn from_env() -> Self {
        Self(
            std::env::var(super::helper::CORE_SERVERS_ENV)
                .ok()
                .map(|raw| {
                    raw.split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect()
                }),
        )
    }

    fn wants(&self, label: &str) -> bool {
        match &self.0 {
            None => true,
            Some(list) => list.iter().any(|s| s == label),
        }
    }
}

/// Every built-in MCP server that this process could configure from its env.
pub struct CoreServer {
    wiki: Option<Child<McpWikiServer>>,
    workspace: Option<Child<McpWorkspaceServer>>,
    docs: Option<Child<McpDocsServer>>,
    memory: Option<Child<McpMemoryServer>>,
    schedule: Option<Child<McpScheduleServer>>,
    background: Option<Child<McpBackgroundServer>>,
    usage: Option<Child<McpUsageServer>>,
    dispatch: Option<Child<McpDispatchServer>>,
    virtual_agents: Option<Child<McpVirtualServer>>,
    space: Option<Child<McpSpaceServer>>,
    send: Option<Child<McpSendServer>>,
    browser: Option<Child<McpBrowserServer>>,
    /// Engine v2 — exactly one of `browser` / `browser_agent` builds.
    browser_agent: Option<Child<McpBrowserAgentServer>>,
    ocr: Option<Child<McpOcrServer>>,
    patterns: Option<Child<McpPatternsServer>>,
    litho: Option<Child<McpLithoServer>>,
    js: Option<Child<McpJsServer>>,
    sandbox: Option<Child<McpSandboxServer>>,
    user_profile: Option<Child<McpUserProfileServer>>,
}

impl CoreServer {
    /// Build every child whose env is present. Never fails as a whole: an
    /// individual child that cannot start is logged and left out.
    pub async fn from_env() -> Result<Self> {
        let w = Wanted::from_env();
        let server = Self {
            wiki: build_child!(
                w,
                "senclaw-wiki",
                McpWikiServer,
                McpWikiServer::from_env().await
            ),
            workspace: build_child!(
                w,
                "senclaw-workspace",
                McpWorkspaceServer,
                McpWorkspaceServer::from_env()
            ),
            docs: build_child!(w, "senclaw-docs", McpDocsServer, McpDocsServer::from_env()),
            memory: build_child!(
                w,
                "senclaw-memory",
                McpMemoryServer,
                McpMemoryServer::from_env()
            ),
            schedule: build_child!(
                w,
                "senclaw-schedule",
                McpScheduleServer,
                McpScheduleServer::from_env()
            ),
            background: build_child!(
                w,
                "senclaw-background",
                McpBackgroundServer,
                McpBackgroundServer::from_env()
            ),
            usage: build_child!(
                w,
                "senclaw-usage",
                McpUsageServer,
                McpUsageServer::from_env()
            ),
            dispatch: build_child!(
                w,
                "senclaw-dispatch",
                McpDispatchServer,
                McpDispatchServer::from_env()
            ),
            virtual_agents: build_child!(
                w,
                "senclaw-virtual",
                McpVirtualServer,
                McpVirtualServer::from_env()
            ),
            space: build_child!(
                w,
                "senclaw-space",
                McpSpaceServer,
                McpSpaceServer::from_env()
            ),
            send: build_child!(w, "senclaw-send", McpSendServer, McpSendServer::from_env()),
            browser: build_child!(
                w,
                "senclaw-browser",
                McpBrowserServer,
                McpBrowserServer::from_env()
            ),
            browser_agent: build_child!(
                w,
                "senclaw-browser",
                McpBrowserAgentServer,
                McpBrowserAgentServer::from_env()
            ),
            ocr: build_child!(w, "senclaw-ocr", McpOcrServer, McpOcrServer::from_env()),
            patterns: build_child!(
                w,
                "senclaw-patterns",
                McpPatternsServer,
                McpPatternsServer::from_env()
            ),
            litho: build_child!(
                w,
                "senclaw-litho",
                McpLithoServer,
                McpLithoServer::from_env()
            ),
            js: build_child!(w, "senclaw-js", McpJsServer, McpJsServer::from_env()),
            sandbox: build_child!(
                w,
                "senclaw-sandbox",
                McpSandboxServer,
                McpSandboxServer::from_env()
            ),
            user_profile: build_child!(
                w,
                "senclaw-profile",
                McpUserProfileServer,
                McpUserProfileServer::from_env()
            ),
        };
        server.warn_on_duplicate_tools();
        Ok(server)
    }

    /// Which children are live, and how many tools each contributes.
    pub fn child_summary(&self) -> Vec<(&'static str, usize)> {
        let mut out = Vec::new();
        for_each_child!(self, child => {
            out.push((child.label, child.router.list_all().len()));
        });
        out
    }

    /// Two children exporting the same tool name would make dispatch depend on
    /// declaration order, which is invisible to whoever added the second tool.
    /// Nothing in the tree does this today; the warning is here so it is caught
    /// the day someone does.
    fn warn_on_duplicate_tools(&self) {
        let mut seen: std::collections::HashMap<String, &'static str> =
            std::collections::HashMap::new();
        for_each_child!(self, child => {
            for tool in child.router.list_all() {
                if let Some(first) = seen.insert(tool.name.to_string(), child.label) {
                    tracing::warn!(
                        "[Core] tool `{}` is exported by both {} and {} — {} wins",
                        tool.name, first, child.label, first,
                    );
                }
            }
        });
    }

    fn status_tool() -> Tool {
        // An empty object schema: the tool takes no arguments.
        let schema = serde_json::json!({ "type": "object", "properties": {} })
            .as_object()
            .cloned()
            .unwrap_or_default();
        Tool::new(
            STATUS_TOOL,
            "List the built-in SenClaw MCP servers bundled into this process and how many tools each contributes.",
            std::sync::Arc::new(schema),
        )
    }

    fn status_result(&self) -> CallToolResult {
        let servers: Vec<serde_json::Value> = self
            .child_summary()
            .into_iter()
            .map(|(name, tools)| serde_json::json!({ "name": name, "tools": tools }))
            .collect();
        let body = serde_json::json!({
            "aggregated": true,
            "servers": servers,
        });
        CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&body).unwrap_or_default(),
        )])
    }
}

impl rmcp::ServerHandler for CoreServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Bundled SenClaw built-in servers (wiki, workspace, memory, …) behind one MCP \
                 endpoint. Call core_status to see which ones are active."
                    .to_string(),
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = vec![Self::status_tool()];
        for_each_child!(self, child => {
            tools.extend(child.router.list_all());
        });
        Ok(ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if name == STATUS_TOOL {
            return Some(Self::status_tool());
        }
        for_each_child!(self, child => {
            if let Some(tool) = child.router.get(name) {
                return Some(tool.clone());
            }
        });
        None
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if request.name == STATUS_TOOL {
            return Ok(self.status_result());
        }
        // `request` is moved into whichever child owns the name; every arm
        // returns, so the borrow checker is happy to let each one try.
        for_each_child!(self, child => {
            if child.router.has_route(&request.name) {
                let ctx = ToolCallContext::new(&child.server, request, context);
                return child.router.call(ctx).await;
            }
        });
        Err(ErrorData::invalid_params(
            format!("tool not found: {}", request.name),
            None,
        ))
    }
}

/// Start the aggregated MCP server over stdio.
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = CoreServer::from_env().await?;
    let summary = server.child_summary();
    tracing::info!(
        "[Core] hosting {} built-in server(s): {}",
        summary.len(),
        summary
            .iter()
            .map(|(name, tools)| format!("{name}({tools})"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ServerHandler;

    /// The handshake must announce `senclaw-core`, not the rmcp crate name.
    ///
    /// Caught by an end-to-end stdio run, not by any in-process test: rmcp
    /// fills `serverInfo` from its own build env unless told otherwise, so the
    /// server introduced itself as "rmcp" while every unit test still passed.
    #[tokio::test]
    async fn handshake_announces_the_senclaw_name() {
        let server = CoreServer::from_env().await.expect("build");
        let info = server.get_info();
        assert_eq!(info.server_info.name, SERVER_NAME);
        assert_eq!(
            SERVER_NAME, "senclaw-core",
            "the user-facing name is what the app's MCP screen keys on"
        );
        assert_ne!(info.server_info.name, "rmcp");
    }

    /// Building with no env at all must still yield a usable server: the
    /// children that need configuration drop out, the ones that don't stay.
    #[tokio::test]
    async fn unconfigured_children_are_skipped_not_fatal() {
        let server = CoreServer::from_env().await.expect("build");
        let summary = server.child_summary();
        // js and litho need no configuration, so they are always present.
        let names: Vec<&str> = summary.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"senclaw-js"), "got {names:?}");
        assert!(names.contains(&"senclaw-litho"), "got {names:?}");
        // Every listed child must actually contribute tools.
        assert!(summary.iter().all(|(_, tools)| *tools > 0), "{summary:?}");
    }

    /// The merged list must contain the aggregator's own tool plus each live
    /// child's tools, with no duplicate names.
    #[tokio::test]
    async fn merged_tool_list_is_unique_and_includes_status() {
        let server = CoreServer::from_env().await.expect("build");
        let mut tools = vec![CoreServer::status_tool()];
        for_each_child!(server, child => {
            tools.extend(child.router.list_all());
        });

        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&STATUS_TOOL.to_string()));

        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(
            unique.len(),
            names.len(),
            "duplicate tool names in {names:?}"
        );
    }

    /// A name that no child exports must be reported as unknown rather than
    /// silently routed to whichever child happens to be first.
    #[tokio::test]
    async fn unknown_tool_is_not_routed_to_a_child() {
        let server = CoreServer::from_env().await.expect("build");
        assert!(server.get_tool("definitely_not_a_tool").is_none());
        // js is always live, so a real name still resolves.
        assert!(server.get_tool("js_eval").is_some());
    }
}

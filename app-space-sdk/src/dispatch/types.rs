//! Shared wire types for the MCP dispatch protocol — used by both the core
//! `MCPDispatcher` engine and any dispatchable source (in-process or a Space App
//! over HTTP). Everything is `serde` so the same types cross the REST contract.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// How many workers the dispatcher can spawn right now.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct Capacity {
    /// Max items to claim across this source this tick.
    pub total: usize,
    /// Max concurrent items per assignee (worker lane). 0 = unlimited.
    pub per_assignee: usize,
}

/// Where a worker runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Workspace {
    /// Fresh temp dir, discarded on completion.
    Scratch,
    /// A persistent absolute path.
    Dir { path: String },
    /// A git worktree for coding tasks.
    Worktree {
        repo: String,
        branch: Option<String>,
    },
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace::Scratch
    }
}

/// An MCP server a worker needs. `Http` specs are bridged to a spawnable stdio
/// server by the engine at launch time (see [`crate::dispatch::run_mcp_bridge`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpServerSpec {
    /// A native stdio MCP server (command + args).
    Stdio {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// An HTTP/SSE MCP server (e.g. a Space App's own MCP). The engine bridges it
    /// to stdio via `senclaw mcp-bridge <url>`.
    Http { name: String, url: String },
}

/// A single dispatchable unit of work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    /// Source-scoped id (opaque to the engine).
    pub id: String,
    /// Worker/persona to route to. `None` = the source's default persona.
    #[serde(default)]
    pub assignee: Option<String>,
    /// The task to run (becomes the agent's user prompt).
    pub prompt: String,
    /// Source-specific system-prompt block appended to the persona's prompt.
    #[serde(default)]
    pub guidance: Option<String>,
    /// MCP servers the worker gets (usually including the source's own tools).
    #[serde(default)]
    pub mcp: Vec<McpServerSpec>,
    /// Where the worker runs.
    #[serde(default)]
    pub workspace: Workspace,
    /// Ids of items this one depends on (already satisfied when returned).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Higher runs first.
    #[serde(default)]
    pub priority: i32,
    /// Per-item run timeout.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// The terminal result of a worker run. The source maps it to its own semantics
/// (e.g. Kanban: `Completed → Done`, `Blocked → Blocked`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Completed {
        #[serde(default)]
        summary: String,
        #[serde(default)]
        metadata: serde_json::Value,
    },
    Blocked {
        reason: String,
    },
    Failed {
        error: String,
    },
    TimedOut,
}

// ---- REST contract DTOs (dispatch_router ⇄ HttpDispatchSource) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollRequest {
    #[serde(default)]
    pub capacity: Capacity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemIdRequest {
    pub item_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalizeRequest {
    pub item_id: String,
    pub outcome: Outcome,
}

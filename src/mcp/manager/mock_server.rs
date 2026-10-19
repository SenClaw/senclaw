//! A just-enough MCP server over HTTP, for tests that need a Space App to be
//! "stopped" at one moment and answering the next.
//!
//! [`reserve`] takes a port without serving on it: connections wait in the
//! kernel's backlog, so the URL is valid and nothing answers — an app whose
//! process is not running. [`Reserved::serve`] starts it, the way the daemon's
//! app proxy starts a session app on the first request.

use std::sync::Arc;

use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};

use crate::mcp::config::McpToolDef;

pub(crate) struct Reserved {
    listener: tokio::net::TcpListener,
    pub(crate) url: String,
}

pub(crate) struct Serving {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub(crate) async fn reserve() -> Reserved {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    Reserved { listener, url }
}

impl Reserved {
    /// Answer `initialize`, `tools/list` (with `tools`) and `tools/call` (an
    /// echo of the tool and its arguments).
    pub(crate) fn serve(self, tools: Vec<McpToolDef>) -> Serving {
        let app = Router::new()
            .route("/mcp", post(rpc))
            .with_state(Arc::new(tools));
        let handle = tokio::spawn(async move {
            let _ = axum::serve(self.listener, app).await;
        });
        Serving { handle }
    }
}

async fn rpc(State(tools): State<Arc<Vec<McpToolDef>>>, Json(req): Json<Value>) -> Json<Value> {
    let result = match req["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mock", "version": "0" },
        }),
        "tools/list" => json!({ "tools": *tools }),
        "tools/call" => {
            let name = req["params"]["name"].as_str().unwrap_or_default();
            let args = &req["params"]["arguments"];
            json!({ "content": [{ "type": "text", "text": format!("{name} {args}") }] })
        }
        _ => json!({}),
    };
    Json(json!({ "jsonrpc": "2.0", "id": req["id"], "result": result }))
}

/// A tool with a schema, as an app reports it.
pub(crate) fn tool(name: &str, description: &str) -> McpToolDef {
    tool_with_schema(
        name,
        description,
        Some(json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
        })),
    )
}

/// A tool declaring exactly `schema` (`None`: the app reports no parameters).
pub(crate) fn tool_with_schema(name: &str, description: &str, schema: Option<Value>) -> McpToolDef {
    McpToolDef {
        name: name.into(),
        description: Some(description.into()),
        input_schema: schema,
    }
}

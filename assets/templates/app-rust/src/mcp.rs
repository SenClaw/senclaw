//! The MCP server: what agents can actually do with this app.
//!
//! JSON-RPC over HTTP POST. Four methods is the whole surface SenClaw's client
//! uses (`initialize`, `ping`, `tools/list`, `tools/call`), so there is no need
//! for an MCP SDK here.
//!
//! Two things decide whether an agent uses a tool well:
//!
//! - **The description.** It is the only thing the model sees when choosing.
//!   Say what the tool does *and when to reach for it*, in the language the
//!   user talks to the agent in.
//! - **Errors that read like sentences.** Returning `isError` with an
//!   explanation tells the agent what to do differently; a JSON-RPC transport
//!   error tells it nothing.
//!
//! Tool names must stay `{{snake_name}}_*` to match `mcp.name`
//! (`{{mcp_name}}`) — the full identifier an agent calls is
//! `mcp__{{mcp_name}}__{{snake_name}}_status`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::Stream;
use serde_json::{json, Value};

use crate::AppState;

/// The SSE half of the transport. The client opens it; this app has nothing to
/// push, so it just stays open.
pub async fn sse(
    State(_state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream =
        futures::stream::once(async { Ok(Event::default().event("endpoint").data("/api/mcp/sse")) });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub async fn message(State(state): State<Arc<AppState>>, Json(req): Json<Value>) -> Json<Value> {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));

    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "{{mcp_name}}", "version": "0.1.0" }
        }),
        // SenClaw sends this as a request with an id rather than a
        // notification, and ignores the reply — but erroring on it looks like a
        // broken server.
        "ping" | "notifications/initialized" | "initialized" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            call(&state, name, &args).await
        }
        _ => json!({}),
    };

    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn tools() -> Value {
    json!([
        {
            "name": "{{snake_name}}_status",
            "description": "Xem {{title_name}} đang chạy ra sao: thời gian hoạt động và số lần mở. Dùng khi người dùng hỏi app còn sống không.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "{{snake_name}}_summarise",
            "description": "Tóm tắt một đoạn văn bản thành đúng ba câu. Dùng khi người dùng đưa một đoạn dài và muốn ý chính.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Đoạn văn bản cần tóm tắt." }
                },
                "required": ["text"]
            }
        }
    ])
}

async fn call(state: &Arc<AppState>, name: &str, args: &Value) -> Value {
    match name {
        "{{snake_name}}_status" => json_result(json!({
            "app": "{{id}}",
            "uptimeSecs": state.started.elapsed().as_secs(),
        })),
        "{{snake_name}}_summarise" => {
            let text = args.get("text").and_then(|t| t.as_str()).unwrap_or("").trim();
            if text.is_empty() {
                return error_result("`text` đang rỗng — truyền đoạn văn bản cần tóm tắt.");
            }
            match state
                .space
                .llm(&format!("Tóm tắt đoạn sau thành đúng ba câu:\n\n{text}"), 600)
                .await
            {
                Ok(out) => text_result(out),
                Err(e) => error_result(&format!("gọi model thất bại: {e}")),
            }
        }
        other => error_result(&format!("không có tool tên {other:?}")),
    }
}

fn text_result(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

fn json_result(v: Value) -> Value {
    text_result(serde_json::to_string_pretty(&v).unwrap_or_default())
}

fn error_result(msg: &str) -> Value {
    json!({ "isError": true, "content": [{ "type": "text", "text": msg }] })
}

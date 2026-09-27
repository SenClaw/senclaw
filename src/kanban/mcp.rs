use axum::{
    extract::State,
    response::sse::{Event, Sse},
    Json,
};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{convert::Infallible, sync::Arc};

use crate::kanban::api::{now, AppState};
use crate::kanban::db::Db;

#[derive(Deserialize, Debug)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

/// Run the Kanban MCP as a NATIVE stdio JSON-RPC server (`senclaw kanban-server`).
/// Speaks newline-delimited JSON-RPC on stdin/stdout, over the same Kanban DB —
/// no HTTP, no bridge. Reuses `tools_list` + `call_tool`.
pub async fn run_stdio_server() -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let state = crate::kanban::api::make_state();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req.id.clone();
        let resp: Value = match req.method.as_str() {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "protocolVersion": "2024-11-05", "capabilities": { "tools": {} },
                            "serverInfo": { "name": "kanban-mcp", "version": "2.0.0" } }
            }),
            "notifications/initialized" => continue, // notification — no reply
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => {
                json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools_list() } })
            }
            "tools/call" => {
                let params = req.params.clone().unwrap_or_default();
                let name = params["name"].as_str().unwrap_or("").to_string();
                let args = params["arguments"].clone();
                let result = call_tool(&state, &name, &args).await;
                json!({ "jsonrpc": "2.0", "id": id, "result": result })
            }
            _ => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        };
        let mut s = serde_json::to_string(&resp)?;
        s.push('\n');
        stdout.write_all(s.as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

pub async fn mcp_sse(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.mcp_tx.subscribe();
    let stream = async_stream::stream! {
        yield Ok(Event::default().event("endpoint").data("/api/mcp/message".to_string()));
        while let Ok(msg) = rx.recv().await {
            yield Ok(Event::default().event("message").data(msg));
        }
    };
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

fn text_result(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}
fn json_result(v: Value) -> Value {
    text_result(serde_json::to_string_pretty(&v).unwrap_or_default())
}
fn error_result(text: String) -> Value {
    json!({ "isError": true, "content": [{ "type": "text", "text": text }] })
}

pub async fn mcp_message(
    State(state): State<Arc<AppState>>,
    Json(req): Json<JsonRpcRequest>,
) -> Json<Value> {
    let reply = |result: Value| -> Json<Value> {
        let resp = json!({ "jsonrpc": "2.0", "id": req.id, "result": result });
        let _ = state.mcp_tx.send(resp.to_string());
        Json(resp)
    };

    match req.method.as_str() {
        "initialize" => reply(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "kanban-mcp", "version": "2.0.0" }
        })),
        "ping" => reply(json!({})),
        "notifications/initialized" => {
            Json(json!({ "jsonrpc": "2.0", "id": req.id, "result": {} }))
        }
        "tools/list" => reply(json!({ "tools": tools_list() })),
        "tools/call" => {
            let params = req.params.clone().unwrap_or_default();
            let name = params["name"].as_str().unwrap_or("").to_string();
            let args = params["arguments"].clone();
            reply(call_tool(&state, &name, &args).await)
        }
        _ => Json(json!("ok")),
    }
}

fn tools_list() -> Value {
    json!([
        {
            "name": "kanban_list_boards",
            "description": "List all Kanban boards (id, title, column/card counts). Start here to find a board to work on.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "kanban_create_board",
            "description": "Create a new Kanban board. By default it is seeded with the Hermes workflow columns: Triage → Todo → Ready → In Progress → Blocked → Done. Set with_defaults=false for an empty board. Returns the new board id.",
            "inputSchema": { "type": "object", "properties": {
                "title": { "type": "string" },
                "description": { "type": "string" },
                "with_defaults": { "type": "boolean", "description": "Seed the Triage→Done workflow columns (default true)" }
            }, "required": ["title"] }
        },
        {
            "name": "kanban_get_board",
            "description": "Get a board's full contents by id: its columns in order (each with its role), each with its cards (id, title, description, priority, assignee, tenant, labels, done, comment/dependency counts).",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "number" } }, "required": ["id"] }
        },
        {
            "name": "kanban_delete_board",
            "description": "Delete a board and all of its columns, cards, comments, and links.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "number" } }, "required": ["id"] }
        },
        {
            "name": "kanban_add_column",
            "description": "Add a workflow column (stage) to a board. `role` is one of triage|todo|ready|in_progress|blocked|done|custom and drives complete/block/unblock semantics. Returns the new column id.",
            "inputSchema": { "type": "object", "properties": {
                "board_id": { "type": "number" },
                "title": { "type": "string" },
                "role": { "type": "string", "enum": ["triage", "todo", "ready", "in_progress", "blocked", "done", "custom"] },
                "color": { "type": "string" },
                "wip_limit": { "type": "number" }
            }, "required": ["board_id", "title"] }
        },
        {
            "name": "kanban_update_column",
            "description": "Rename a column or change its color / WIP limit.",
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "number" },
                "title": { "type": "string" },
                "color": { "type": "string" },
                "wip_limit": { "type": "number" }
            }, "required": ["id"] }
        },
        {
            "name": "kanban_delete_column",
            "description": "Delete a column and all cards in it.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "number" } }, "required": ["id"] }
        },
        {
            "name": "kanban_create",
            "description": "Create a task card on a board. Routes to `column_id` if given, else the `status` column role, else Todo (or the first column). Set `assignee` to a worker/profile name to route it into that worker's lane. Returns the new card id.",
            "inputSchema": { "type": "object", "properties": {
                "board_id": { "type": "number" },
                "title": { "type": "string", "description": "Short, actionable task title" },
                "description": { "type": "string" },
                "column_id": { "type": "number", "description": "Explicit destination column" },
                "status": { "type": "string", "enum": ["triage", "todo", "ready", "in_progress", "blocked", "done"], "description": "Destination column by role (if column_id omitted)" },
                "assignee": { "type": "string", "description": "Worker / profile name to route the task to" },
                "priority": { "type": "string", "enum": ["low", "medium", "high", "urgent"] },
                "tenant": { "type": "string", "description": "Optional tenant namespace" },
                "labels": { "type": "array", "items": { "type": "string" } }
            }, "required": ["board_id", "title"] }
        },
        {
            "name": "kanban_show",
            "description": "Show a single task with its full context: fields, its comment thread, and its dependency links (blocked-by parents and child tasks). Use before acting on a task.",
            "inputSchema": { "type": "object", "properties": { "card_id": { "type": "number" } }, "required": ["card_id"] }
        },
        {
            "name": "kanban_list",
            "description": "List a board's tasks, optionally filtered by column role, assignee, or tenant. Great for a worker to find its assigned work.",
            "inputSchema": { "type": "object", "properties": {
                "board_id": { "type": "number" },
                "role": { "type": "string", "enum": ["triage", "todo", "ready", "in_progress", "blocked", "done"] },
                "assignee": { "type": "string" },
                "tenant": { "type": "string" }
            }, "required": ["board_id"] }
        },
        {
            "name": "kanban_update_card",
            "description": "Edit a task's fields (title, description, priority, assignee, tenant, labels array, done).",
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "number" },
                "title": { "type": "string" },
                "description": { "type": "string" },
                "priority": { "type": "string", "enum": ["low", "medium", "high", "urgent"] },
                "assignee": { "type": "string" },
                "tenant": { "type": "string" },
                "labels": { "type": "array", "items": { "type": "string" } },
                "done": { "type": "boolean" }
            }, "required": ["id"] }
        },
        {
            "name": "kanban_move_card",
            "description": "Move a task to a column (destination column_id) at a 0-based position (index). Advances a task through the workflow. Moving into a `done` column marks it complete.",
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "number" },
                "column_id": { "type": "number" },
                "index": { "type": "number" }
            }, "required": ["id", "column_id"] }
        },
        {
            "name": "kanban_complete",
            "description": "Finish a task: move it to the board's Done column and record a completion summary as a comment. This is how a worker signals success.",
            "inputSchema": { "type": "object", "properties": {
                "card_id": { "type": "number" },
                "summary": { "type": "string", "description": "Narrative outcome / what was done" }
            }, "required": ["card_id"] }
        },
        {
            "name": "kanban_block",
            "description": "Block a task: move it to the Blocked column and record the reason as a comment. Use when human input or an external dependency is needed.",
            "inputSchema": { "type": "object", "properties": {
                "card_id": { "type": "number" },
                "reason": { "type": "string", "description": "Why the task is blocked / what's needed to unblock" }
            }, "required": ["card_id"] }
        },
        {
            "name": "kanban_unblock",
            "description": "Resume a blocked task: move it back to Ready (or Todo) and log a note.",
            "inputSchema": { "type": "object", "properties": {
                "card_id": { "type": "number" },
                "note": { "type": "string" }
            }, "required": ["card_id"] }
        },
        {
            "name": "kanban_comment",
            "description": "Append a durable note to a task's comment thread (inter-agent + human-agent protocol).",
            "inputSchema": { "type": "object", "properties": {
                "card_id": { "type": "number" },
                "body": { "type": "string" },
                "author": { "type": "string", "description": "Who is commenting (profile / name)" }
            }, "required": ["card_id", "body"] }
        },
        {
            "name": "kanban_link",
            "description": "Add a dependency: `parent_id` must finish before `child_id`. The child shows as blocked while any parent is not done; the parent shows child-progress. Pass remove=true to delete the link.",
            "inputSchema": { "type": "object", "properties": {
                "parent_id": { "type": "number" },
                "child_id": { "type": "number" },
                "remove": { "type": "boolean" }
            }, "required": ["parent_id", "child_id"] }
        },
        {
            "name": "kanban_delete_card",
            "description": "Delete a task.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "number" } }, "required": ["id"] }
        },
        {
            "name": "kanban_generate_board",
            "description": "AI-plan a whole board (workflow columns + task cards) from a project goal, in one step. Creates a fresh board unless board_id is given (then appends the generated columns into it).",
            "inputSchema": { "type": "object", "properties": {
                "goal": { "type": "string" },
                "instruction": { "type": "string" },
                "title": { "type": "string" },
                "board_id": { "type": "number" }
            }, "required": ["goal"] }
        },
        {
            "name": "kanban_breakdown_card",
            "description": "AI-break a task into concrete subtask cards, inserted into the same column right after it.",
            "inputSchema": { "type": "object", "properties": {
                "card_id": { "type": "number" },
                "instruction": { "type": "string" }
            }, "required": ["card_id"] }
        }
    ])
}

fn labels_arg(args: &Value) -> Option<String> {
    args.get("labels").and_then(|v| v.as_array()).map(|a| {
        let strs: Vec<String> = a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        serde_json::to_string(&strs).unwrap_or_else(|_| "[]".into())
    })
}

/// Resolve the destination column for `kanban_create`: explicit id → status role
/// → `todo` role → the board's first column.
fn resolve_column(
    db: &Db,
    board_id: i64,
    column_id: Option<i64>,
    status: Option<&str>,
) -> anyhow::Result<i64> {
    if let Some(id) = column_id {
        return Ok(id);
    }
    if let Some(role) = status {
        if let Some(id) = db.column_by_role(board_id, role)? {
            return Ok(id);
        }
    }
    if let Some(id) = db.column_by_role(board_id, "todo")? {
        return Ok(id);
    }
    let cols = db.board_full(board_id)?;
    cols.first()
        .map(|c| c.column.id)
        .ok_or_else(|| anyhow::anyhow!("board {board_id} has no columns; add one first"))
}

/// Shared complete/block/unblock: move to the role column + log a comment.
fn transition(db: &Db, card_id: i64, role: &str, kind: &str, body: &str) -> anyhow::Result<bool> {
    let (_t, _d, _c, board_id) = db.card_detail(card_id)?;
    let moved = if let Some(dest) = db.column_by_role(board_id, role)? {
        db.move_card(card_id, dest, 0, now())?;
        true
    } else if role == "done" {
        db.update_card(
            card_id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(true),
            now(),
        )?;
        false
    } else {
        false
    };
    if !body.trim().is_empty() {
        db.add_comment(card_id, "agent", body.trim(), kind, now())?;
    }
    Ok(moved)
}

async fn call_tool(state: &Arc<AppState>, name: &str, args: &Value) -> Value {
    let db = &state.db;
    match name {
        "kanban_list_boards" => match db.list_boards() {
            Ok(v) => json_result(json!(v)),
            Err(e) => error_result(e.to_string()),
        },
        "kanban_create_board" => {
            let title = args["title"].as_str().unwrap_or("").trim();
            if title.is_empty() {
                return error_result("title is required".into());
            }
            let desc = args["description"].as_str().unwrap_or("");
            let with_defaults = args["with_defaults"].as_bool().unwrap_or(true);
            let ws = args["workspace_dir"]
                .as_str()
                .filter(|w| !w.trim().is_empty());
            match db.create_board(title, desc, with_defaults, ws, now()) {
                Ok(id) => json_result(json!({ "id": id })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_get_board" => {
            let id = args["id"].as_i64().unwrap_or(0);
            match (db.board_meta(id), db.board_full(id)) {
                (Ok(Some(meta)), Ok(cols)) => json_result(json!({ "meta": meta, "columns": cols })),
                (Ok(None), _) => error_result(format!("board {id} not found")),
                (Err(e), _) | (_, Err(e)) => error_result(e.to_string()),
            }
        }
        "kanban_delete_board" => {
            let id = args["id"].as_i64().unwrap_or(0);
            match db.delete_board(id) {
                Ok(()) => json_result(json!({ "success": true })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_add_column" => {
            let board_id = args["board_id"].as_i64().unwrap_or(0);
            let title = args["title"].as_str().unwrap_or("").trim();
            if title.is_empty() {
                return error_result("title is required".into());
            }
            let role = args["role"].as_str().unwrap_or("custom");
            let color = args["color"].as_str();
            let wip = args["wip_limit"].as_i64();
            match db.add_column(board_id, title, role, color, wip, now()) {
                Ok(id) => json_result(json!({ "id": id })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_update_column" => {
            let id = args["id"].as_i64().unwrap_or(0);
            let title = args["title"].as_str();
            let color = args.get("color").map(|c| c.as_str());
            let wip = args.get("wip_limit").map(|c| c.as_i64());
            match db.update_column(id, title, color, wip, now()) {
                Ok(()) => json_result(json!({ "success": true })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_delete_column" => {
            let id = args["id"].as_i64().unwrap_or(0);
            match db.delete_column(id, now()) {
                Ok(()) => json_result(json!({ "success": true })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_create" => {
            let board_id = args["board_id"].as_i64().unwrap_or(0);
            let title = args["title"].as_str().unwrap_or("").trim();
            if title.is_empty() {
                return error_result("title is required".into());
            }
            let column_id = args["column_id"].as_i64();
            let status = args["status"].as_str();
            let column = match resolve_column(db, board_id, column_id, status) {
                Ok(c) => c,
                Err(e) => return error_result(e.to_string()),
            };
            let desc = args["description"].as_str().unwrap_or("");
            let priority = args["priority"].as_str();
            let assignee = args["assignee"].as_str();
            let tenant = args["tenant"].as_str();
            let labels = labels_arg(args);
            match db.add_card(
                column,
                title,
                desc,
                priority,
                assignee,
                tenant,
                labels.as_deref(),
                None,
                now(),
            ) {
                Ok(id) => json_result(json!({ "id": id, "column_id": column })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_show" => {
            let card_id = args["card_id"].as_i64().unwrap_or(0);
            match db.card_row(card_id) {
                Ok(Some(card)) => {
                    let comments = db.comments_of_card(card_id).unwrap_or_default();
                    let links = db.links_of_card(card_id).unwrap_or_default();
                    json_result(json!({ "card": card, "comments": comments, "links": links }))
                }
                Ok(None) => error_result(format!("card {card_id} not found")),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_list" => {
            let board_id = args["board_id"].as_i64().unwrap_or(0);
            let role = args["role"].as_str();
            let assignee = args["assignee"].as_str();
            let tenant = args["tenant"].as_str();
            match db.list_cards(board_id, role, assignee, tenant) {
                Ok(cards) => json_result(json!(cards)),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_update_card" => {
            let id = args["id"].as_i64().unwrap_or(0);
            let title = args["title"].as_str();
            let desc = args["description"].as_str();
            let priority = args.get("priority").map(|c| c.as_str());
            let assignee = args.get("assignee").map(|c| c.as_str());
            let tenant = args.get("tenant").map(|c| c.as_str());
            let labels = args
                .get("labels")
                .map(|_| labels_arg(args).unwrap_or_else(|| "[]".into()));
            let labels_ref = labels.as_ref().map(|s| Some(s.as_str()));
            let done = args["done"].as_bool();
            match db.update_card(
                id,
                title,
                desc,
                priority,
                assignee,
                tenant,
                labels_ref,
                None,
                done,
                now(),
            ) {
                Ok(()) => json_result(json!({ "success": true })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_move_card" => {
            let id = args["id"].as_i64().unwrap_or(0);
            let column_id = args["column_id"].as_i64().unwrap_or(0);
            let index = args["index"].as_i64().unwrap_or(0);
            match db.move_card(id, column_id, index, now()) {
                Ok(()) => json_result(json!({ "success": true })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_complete" => {
            let card_id = args["card_id"].as_i64().unwrap_or(0);
            let summary = args["summary"].as_str().unwrap_or("");
            match transition(db, card_id, "done", "complete", summary) {
                Ok(moved) => json_result(json!({ "success": true, "moved": moved })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_block" => {
            let card_id = args["card_id"].as_i64().unwrap_or(0);
            let reason = args["reason"].as_str().unwrap_or("");
            match transition(db, card_id, "blocked", "block", reason) {
                Ok(moved) => json_result(json!({ "success": true, "moved": moved })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_unblock" => {
            let card_id = args["card_id"].as_i64().unwrap_or(0);
            let note = args["note"].as_str().unwrap_or("");
            let board_id = match db.card_detail(card_id) {
                Ok((_, _, _, b)) => b,
                Err(e) => return error_result(e.to_string()),
            };
            let target = match db.column_by_role(board_id, "ready") {
                Ok(Some(_)) => "ready",
                _ => "todo",
            };
            match transition(db, card_id, target, "unblock", note) {
                Ok(moved) => json_result(json!({ "success": true, "moved": moved })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_comment" => {
            let card_id = args["card_id"].as_i64().unwrap_or(0);
            let body = args["body"].as_str().unwrap_or("").trim();
            if body.is_empty() {
                return error_result("body is required".into());
            }
            let author = args["author"]
                .as_str()
                .filter(|a| !a.trim().is_empty())
                .unwrap_or("agent");
            match db.add_comment(card_id, author, body, "comment", now()) {
                Ok(id) => json_result(json!({ "id": id })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_link" => {
            let parent = args["parent_id"].as_i64().unwrap_or(0);
            let child = args["child_id"].as_i64().unwrap_or(0);
            let remove = args["remove"].as_bool().unwrap_or(false);
            let res = if remove {
                db.remove_link(parent, child, now())
                    .map(|_| json!({ "success": true, "removed": true }))
            } else {
                db.add_link(parent, child, now())
                    .map(|id| json!({ "id": id }))
            };
            match res {
                Ok(v) => json_result(v),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_delete_card" => {
            let id = args["id"].as_i64().unwrap_or(0);
            match db.delete_card(id, now()) {
                Ok(()) => json_result(json!({ "success": true })),
                Err(e) => error_result(e.to_string()),
            }
        }
        "kanban_generate_board" => {
            let goal = args["goal"].as_str().unwrap_or("").trim();
            if goal.is_empty() {
                return error_result("goal is required".into());
            }
            let instruction = args["instruction"].as_str();
            match crate::kanban::llm::generate_board(goal, instruction).await {
                Ok(gen) => {
                    let board_id = match args["board_id"].as_i64() {
                        Some(id) if id > 0 => id,
                        _ => {
                            let title = args["title"]
                                .as_str()
                                .map(str::trim)
                                .filter(|t| !t.is_empty())
                                .unwrap_or(goal);
                            match db.create_board(title, goal, false, None, now()) {
                                Ok(id) => id,
                                Err(e) => return error_result(e.to_string()),
                            }
                        }
                    };
                    match db.insert_columns(board_id, &gen.columns, now()) {
                        Ok((cols, cards)) => json_result(
                            json!({ "boardId": board_id, "columns": cols, "cards": cards, "model": gen.model }),
                        ),
                        Err(e) => error_result(e.to_string()),
                    }
                }
                Err(e) => error_result(e),
            }
        }
        "kanban_breakdown_card" => {
            let card_id = args["card_id"].as_i64().unwrap_or(0);
            let (title, description, column_id, board_id) = match db.card_detail(card_id) {
                Ok(t) => t,
                Err(e) => return error_result(e.to_string()),
            };
            let outline = db.board_outline(board_id).ok();
            let instruction = args["instruction"].as_str();
            match crate::kanban::llm::breakdown_card(
                &title,
                &description,
                outline.as_deref(),
                instruction,
            )
            .await
            {
                Ok(gen) => match db.insert_cards(column_id, &gen.cards, now()) {
                    Ok(added) => json_result(json!({ "added": added, "model": gen.model })),
                    Err(e) => error_result(e.to_string()),
                },
                Err(e) => error_result(e),
            }
        }
        _ => error_result(format!("Unknown tool: {name}")),
    }
}

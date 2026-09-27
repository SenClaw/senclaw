use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::kanban::db::{default_data_dir, Db};
use crate::kanban::llm::{self, ChatBody};

pub struct AppState {
    pub db: Arc<Db>,
    /// Broadcasts the raw JSON-RPC responses to any connected MCP SSE clients.
    pub mcp_tx: tokio::sync::broadcast::Sender<String>,
}

pub struct ApiError(pub StatusCode, pub String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}
fn bad(e: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, e.to_string())
}
fn gateway(e: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::BAD_GATEWAY, e.to_string())
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn make_state() -> Arc<AppState> {
    let db_path = default_data_dir("kanban").join("kanban.db");
    let db = Arc::new(Db::open(&db_path).expect("open kanban db"));
    let (mcp_tx, _) = tokio::sync::broadcast::channel(100);
    Arc::new(AppState { db, mcp_tx })
}

pub fn api_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/llm-info", get(llm_info))
        .route("/boards", get(list_boards).post(create_board))
        .route("/templates", get(list_templates).post(save_template))
        .route("/templates/delete", post(delete_template))
        .route("/activity", get(activity))
        .route("/board", get(get_board))
        .route("/board/rename", post(rename_board))
        .route("/board/delete", post(delete_board))
        .route("/assignees", get(assignees))
        .route("/column/add", post(add_column))
        .route("/column/update", post(update_column))
        .route("/column/delete", post(delete_column))
        .route("/column/reorder", post(reorder_columns))
        .route("/card", get(get_card))
        .route("/card/add", post(add_card))
        .route("/card/update", post(update_card))
        .route("/card/move", post(move_card))
        .route("/card/delete", post(delete_card))
        .route("/card/complete", post(complete_card))
        .route("/card/block", post(block_card))
        .route("/card/unblock", post(unblock_card))
        .route("/card/comment", post(add_comment))
        .route("/link/add", post(add_link))
        .route("/link/remove", post(remove_link))
        .route("/generate", post(generate_board))
        .route("/breakdown", post(breakdown_card))
        .route("/chat", post(chat))
        .route("/chat/sessions", get(list_sessions).post(create_session))
        .route("/chat/session/rename", post(rename_session))
        .route("/chat/session/delete", post(delete_session))
        .route("/chat/messages", get(session_messages))
        .route("/models", get(models))
        .route("/model-active", post(model_active))
        .route(
            "/mcp/sse",
            get(crate::kanban::mcp::mcp_sse).post(crate::kanban::mcp::mcp_message),
        )
        .route("/mcp/message", post(crate::kanban::mcp::mcp_message))
        .with_state(state)
}

async fn status() -> Json<Value> {
    Json(json!({ "ok": true, "app": "kanban" }))
}

// ---- boards ----

async fn list_boards(State(s): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!(s.db.list_boards().map_err(bad)?)))
}

#[derive(Deserialize)]
struct CreateBoardBody {
    title: String,
    #[serde(default)]
    description: String,
    /// Seed the Hermes workflow columns (default true; ignored when template_id set).
    #[serde(default = "yes")]
    with_defaults: bool,
    /// Working directory for this board's dispatched workers (outputs land here).
    #[serde(default)]
    workspace_dir: Option<String>,
    /// Column template to seed from (builtin `standard`/`advanced`/`simple` or a
    /// custom template id). Overrides `with_defaults`.
    #[serde(default)]
    template_id: Option<String>,
}
fn yes() -> bool {
    true
}

async fn create_board(
    State(s): State<Arc<AppState>>,
    Json(b): Json<CreateBoardBody>,
) -> Result<Json<Value>, ApiError> {
    let title = if b.title.trim().is_empty() {
        "Untitled board"
    } else {
        b.title.trim()
    };
    let ws = b
        .workspace_dir
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty());
    let id = match b.template_id.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(tid) => {
            let tpl =
                s.db.get_template(tid)
                    .map_err(bad)?
                    .ok_or_else(|| bad(format!("unknown template: {tid}")))?;
            s.db.create_board_from_template(title, b.description.trim(), ws, &tpl, now())
                .map_err(bad)?
        }
        None => {
            s.db.create_board(title, b.description.trim(), b.with_defaults, ws, now())
                .map_err(bad)?
        }
    };
    Ok(Json(json!({ "id": id })))
}

// ---- column templates ----

async fn list_templates(State(s): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!(s.db.list_templates().map_err(bad)?)))
}

#[derive(Deserialize)]
struct SaveTemplateBody {
    name: String,
    #[serde(default)]
    description: String,
    columns: Vec<crate::kanban::templates::TemplateColumn>,
}

/// Create or import a custom template (import = POST an exported JSON body).
async fn save_template(
    State(s): State<Arc<AppState>>,
    Json(b): Json<SaveTemplateBody>,
) -> Result<Json<Value>, ApiError> {
    let id =
        s.db.save_template(&b.name, &b.description, &b.columns)
            .map_err(bad)?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct DeleteTemplateBody {
    id: String,
}

async fn delete_template(
    State(s): State<Arc<AppState>>,
    Json(b): Json<DeleteTemplateBody>,
) -> Result<Json<Value>, ApiError> {
    s.db.delete_template(&b.id).map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

/// Live activity for a board: currently-running tasks + the recent worker feed.
async fn activity(
    State(s): State<Arc<AppState>>,
    Query(q): Query<BoardIdQuery>,
) -> Result<Json<Value>, ApiError> {
    let running = s.db.activity_running(q.board_id).map_err(bad)?;
    let recent = s.db.activity_recent(q.board_id, 30).map_err(bad)?;
    Ok(Json(json!({ "running": running, "recent": recent })))
}

#[derive(Deserialize)]
struct BoardQuery {
    id: i64,
}

async fn get_board(
    State(s): State<Arc<AppState>>,
    Query(q): Query<BoardQuery>,
) -> Result<Json<Value>, ApiError> {
    let meta =
        s.db.board_meta(q.id)
            .map_err(bad)?
            .ok_or_else(|| bad("board not found"))?;
    let columns = s.db.board_full(q.id).map_err(bad)?;
    Ok(Json(json!({ "meta": meta, "columns": columns })))
}

#[derive(Deserialize)]
struct RenameBoardBody {
    id: i64,
    title: String,
    #[serde(default)]
    description: String,
}

async fn rename_board(
    State(s): State<Arc<AppState>>,
    Json(b): Json<RenameBoardBody>,
) -> Result<Json<Value>, ApiError> {
    s.db.rename_board(b.id, b.title.trim(), b.description.trim(), now())
        .map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

async fn delete_board(
    State(s): State<Arc<AppState>>,
    Json(b): Json<BoardQuery>,
) -> Result<Json<Value>, ApiError> {
    s.db.delete_board(b.id).map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
struct BoardIdQuery {
    board_id: i64,
}

async fn assignees(
    State(s): State<Arc<AppState>>,
    Query(q): Query<BoardIdQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!(s.db.assignees(q.board_id).map_err(bad)?)))
}

// ---- columns ----

#[derive(Deserialize)]
struct AddColumnBody {
    board_id: i64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    wip_limit: Option<i64>,
}

async fn add_column(
    State(s): State<Arc<AppState>>,
    Json(b): Json<AddColumnBody>,
) -> Result<Json<Value>, ApiError> {
    let title = if b.title.trim().is_empty() {
        "New stage"
    } else {
        b.title.trim()
    };
    let role = b.role.as_deref().unwrap_or("custom");
    let id =
        s.db.add_column(
            b.board_id,
            title,
            role,
            b.color.as_deref(),
            b.wip_limit,
            now(),
        )
        .map_err(bad)?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct UpdateColumnBody {
    id: i64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, deserialize_with = "double_option_str")]
    color: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option_i64")]
    wip_limit: Option<Option<i64>>,
}

async fn update_column(
    State(s): State<Arc<AppState>>,
    Json(b): Json<UpdateColumnBody>,
) -> Result<Json<Value>, ApiError> {
    let color = b.color.as_ref().map(|o| o.as_deref());
    s.db.update_column(b.id, b.title.as_deref(), color, b.wip_limit, now())
        .map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
struct IdBody {
    id: i64,
}

async fn delete_column(
    State(s): State<Arc<AppState>>,
    Json(b): Json<IdBody>,
) -> Result<Json<Value>, ApiError> {
    s.db.delete_column(b.id, now()).map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
struct ReorderBody {
    board_id: i64,
    ids: Vec<i64>,
}

async fn reorder_columns(
    State(s): State<Arc<AppState>>,
    Json(b): Json<ReorderBody>,
) -> Result<Json<Value>, ApiError> {
    s.db.reorder_columns(b.board_id, &b.ids, now())
        .map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

// ---- cards ----

#[derive(Deserialize)]
struct CardIdQuery {
    id: i64,
}

/// Full detail for one card: the card, its comment thread, and its dependency links.
async fn get_card(
    State(s): State<Arc<AppState>>,
    Query(q): Query<CardIdQuery>,
) -> Result<Json<Value>, ApiError> {
    let card =
        s.db.card_row(q.id)
            .map_err(bad)?
            .ok_or_else(|| bad("card not found"))?;
    let comments = s.db.comments_of_card(q.id).map_err(bad)?;
    let links = s.db.links_of_card(q.id).map_err(bad)?;
    Ok(Json(
        json!({ "card": card, "comments": comments, "links": links }),
    ))
}

#[derive(Deserialize)]
struct AddCardBody {
    column_id: i64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    tenant: Option<String>,
    #[serde(default)]
    labels: Option<Vec<String>>,
    #[serde(default)]
    due_date: Option<i64>,
}

async fn add_card(
    State(s): State<Arc<AppState>>,
    Json(b): Json<AddCardBody>,
) -> Result<Json<Value>, ApiError> {
    let title = if b.title.trim().is_empty() {
        "New task"
    } else {
        b.title.trim()
    };
    let labels = b
        .labels
        .as_ref()
        .and_then(|v| serde_json::to_string(v).ok());
    let id =
        s.db.add_card(
            b.column_id,
            title,
            b.description.trim(),
            b.priority.as_deref(),
            b.assignee.as_deref(),
            b.tenant.as_deref(),
            labels.as_deref(),
            b.due_date,
            now(),
        )
        .map_err(bad)?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct UpdateCardBody {
    id: i64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, deserialize_with = "double_option_str")]
    priority: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option_str")]
    assignee: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option_str")]
    tenant: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option_labels")]
    labels: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option_i64")]
    due_date: Option<Option<i64>>,
    #[serde(default)]
    done: Option<bool>,
}

async fn update_card(
    State(s): State<Arc<AppState>>,
    Json(b): Json<UpdateCardBody>,
) -> Result<Json<Value>, ApiError> {
    let priority = b.priority.as_ref().map(|o| o.as_deref());
    let assignee = b.assignee.as_ref().map(|o| o.as_deref());
    let tenant = b.tenant.as_ref().map(|o| o.as_deref());
    let labels = b.labels.as_ref().map(|o| o.as_deref());
    s.db.update_card(
        b.id,
        b.title.as_deref(),
        b.description.as_deref(),
        priority,
        assignee,
        tenant,
        labels,
        b.due_date,
        b.done,
        now(),
    )
    .map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
struct MoveCardBody {
    id: i64,
    column_id: i64,
    #[serde(default)]
    index: i64,
}

async fn move_card(
    State(s): State<Arc<AppState>>,
    Json(b): Json<MoveCardBody>,
) -> Result<Json<Value>, ApiError> {
    s.db.move_card(b.id, b.column_id, b.index, now())
        .map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

async fn delete_card(
    State(s): State<Arc<AppState>>,
    Json(b): Json<IdBody>,
) -> Result<Json<Value>, ApiError> {
    s.db.delete_card(b.id, now()).map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

/// Shared helper: move a card into the board's column with the given `role`,
/// optionally logging a comment. Returns whether a matching column was found.
fn transition(
    db: &Db,
    card_id: i64,
    role: &str,
    comment: Option<(&str, &str)>,
) -> anyhow::Result<bool> {
    let (_t, _d, _col, board_id) = db.card_detail(card_id)?;
    let moved = if let Some(dest) = db.column_by_role(board_id, role)? {
        db.move_card(card_id, dest, 0, now())?;
        true
    } else {
        false
    };
    if let Some((kind, body)) = comment {
        if !body.trim().is_empty() {
            db.add_comment(card_id, "agent", body.trim(), kind, now())?;
        }
    }
    Ok(moved)
}

#[derive(Deserialize)]
struct CompleteBody {
    card_id: i64,
    #[serde(default)]
    summary: Option<String>,
}

/// Mark a card complete: move to the `done` column and record a summary comment.
async fn complete_card(
    State(s): State<Arc<AppState>>,
    Json(b): Json<CompleteBody>,
) -> Result<Json<Value>, ApiError> {
    let summary = b.summary.as_deref().unwrap_or("");
    let moved = transition(&s.db, b.card_id, "done", Some(("complete", summary))).map_err(bad)?;
    if !moved {
        // No done column — just flag done.
        s.db.update_card(
            b.card_id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(true),
            now(),
        )
        .map_err(bad)?;
    }
    Ok(Json(json!({ "success": true, "moved": moved })))
}

#[derive(Deserialize)]
struct BlockBody {
    card_id: i64,
    #[serde(default)]
    reason: Option<String>,
}

async fn block_card(
    State(s): State<Arc<AppState>>,
    Json(b): Json<BlockBody>,
) -> Result<Json<Value>, ApiError> {
    let reason = b.reason.as_deref().unwrap_or("");
    let moved = transition(&s.db, b.card_id, "blocked", Some(("block", reason))).map_err(bad)?;
    Ok(Json(json!({ "success": true, "moved": moved })))
}

#[derive(Deserialize)]
struct UnblockBody {
    card_id: i64,
    #[serde(default)]
    note: Option<String>,
}

/// Resume a blocked card: move it back to `ready` (or `todo`) and log a note.
async fn unblock_card(
    State(s): State<Arc<AppState>>,
    Json(b): Json<UnblockBody>,
) -> Result<Json<Value>, ApiError> {
    let note = b.note.as_deref().unwrap_or("");
    // Prefer the `ready` column, fall back to `todo`.
    let (_t, _d, _c, board_id) = s.db.card_detail(b.card_id).map_err(bad)?;
    let target = if s
        .db
        .column_by_role(board_id, "ready")
        .map_err(bad)?
        .is_some()
    {
        "ready"
    } else {
        "todo"
    };
    let moved = transition(&s.db, b.card_id, target, Some(("unblock", note))).map_err(bad)?;
    Ok(Json(json!({ "success": true, "moved": moved })))
}

#[derive(Deserialize)]
struct CommentBody {
    card_id: i64,
    body: String,
    #[serde(default)]
    author: Option<String>,
}

async fn add_comment(
    State(s): State<Arc<AppState>>,
    Json(b): Json<CommentBody>,
) -> Result<Json<Value>, ApiError> {
    let body = b.body.trim();
    if body.is_empty() {
        return Err(bad("empty comment"));
    }
    let author = b
        .author
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .unwrap_or("Bạn");
    let id =
        s.db.add_comment(b.card_id, author, body, "comment", now())
            .map_err(bad)?;
    Ok(Json(json!({ "id": id })))
}

// ---- dependency links ----

#[derive(Deserialize)]
struct LinkBody {
    parent_id: i64,
    child_id: i64,
}

async fn add_link(
    State(s): State<Arc<AppState>>,
    Json(b): Json<LinkBody>,
) -> Result<Json<Value>, ApiError> {
    let id = s.db.add_link(b.parent_id, b.child_id, now()).map_err(bad)?;
    Ok(Json(json!({ "id": id })))
}

async fn remove_link(
    State(s): State<Arc<AppState>>,
    Json(b): Json<LinkBody>,
) -> Result<Json<Value>, ApiError> {
    s.db.remove_link(b.parent_id, b.child_id, now())
        .map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

// ---- AI ----

#[derive(Deserialize)]
struct GenerateBody {
    #[serde(default)]
    board_id: Option<i64>,
    goal: String,
    #[serde(default)]
    instruction: Option<String>,
    #[serde(default)]
    title: Option<String>,
    /// Working directory for the new board's workers.
    #[serde(default)]
    workspace_dir: Option<String>,
    /// Column template: None or "ai" = AI generates the columns too; otherwise a
    /// template id — columns come from the template and AI generates only the
    /// task cards (placed in the template's Todo column).
    #[serde(default)]
    template_id: Option<String>,
}

async fn generate_board(
    State(s): State<Arc<AppState>>,
    Json(b): Json<GenerateBody>,
) -> Result<Json<Value>, ApiError> {
    if b.goal.trim().is_empty() {
        return Err(bad("goal is required"));
    }
    let ws = b
        .workspace_dir
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty());
    let title_owned = b
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| b.goal.trim())
        .to_string();

    let template = match b.template_id.as_deref().map(str::trim) {
        Some("") | Some("ai") | None => None,
        Some(tid) => Some(
            s.db.get_template(tid)
                .map_err(bad)?
                .ok_or_else(|| bad(format!("unknown template: {tid}")))?,
        ),
    };

    match template {
        // Template columns + AI-generated cards into the template's Todo column.
        Some(tpl) => {
            let gen = llm::generate_cards(b.goal.trim(), b.instruction.as_deref())
                .await
                .map_err(gateway)?;
            let board_id = match b.board_id {
                Some(id) => id,
                None => {
                    s.db.create_board_from_template(&title_owned, b.goal.trim(), ws, &tpl, now())
                        .map_err(bad)?
                }
            };
            let todo =
                s.db.column_by_role(board_id, "todo")
                    .map_err(bad)?
                    .or_else(|| s.db.board_full(board_id).ok()?.first().map(|c| c.column.id))
                    .ok_or_else(|| bad("board has no columns"))?;
            let added = s.db.insert_cards(todo, &gen.cards, now()).map_err(bad)?;
            Ok(Json(
                json!({ "boardId": board_id, "columns": tpl.columns.len(), "cards": added, "model": gen.model }),
            ))
        }
        // Fully AI-generated (columns + cards).
        None => {
            let gen = llm::generate_board(b.goal.trim(), b.instruction.as_deref())
                .await
                .map_err(gateway)?;
            let board_id = match b.board_id {
                Some(id) => id,
                None => {
                    s.db.create_board(&title_owned, b.goal.trim(), false, ws, now())
                        .map_err(bad)?
                }
            };
            let (cols, cards) =
                s.db.insert_columns(board_id, &gen.columns, now())
                    .map_err(bad)?;
            Ok(Json(
                json!({ "boardId": board_id, "columns": cols, "cards": cards, "model": gen.model }),
            ))
        }
    }
}

#[derive(Deserialize)]
struct BreakdownBody {
    card_id: i64,
    #[serde(default)]
    instruction: Option<String>,
}

async fn breakdown_card(
    State(s): State<Arc<AppState>>,
    Json(b): Json<BreakdownBody>,
) -> Result<Json<Value>, ApiError> {
    let (title, description, column_id, board_id) = s.db.card_detail(b.card_id).map_err(bad)?;
    let outline = s.db.board_outline(board_id).ok();
    let gen = llm::breakdown_card(
        &title,
        &description,
        outline.as_deref(),
        b.instruction.as_deref(),
    )
    .await
    .map_err(gateway)?;
    let added =
        s.db.insert_cards(column_id, &gen.cards, now())
            .map_err(bad)?;
    Ok(Json(json!({ "added": added, "model": gen.model })))
}

// ---- chat ----

#[derive(Deserialize)]
struct ChatSendBody {
    session_id: i64,
    content: String,
    #[serde(default)]
    board_outline: Option<String>,
}

async fn chat(
    State(s): State<Arc<AppState>>,
    Json(b): Json<ChatSendBody>,
) -> Result<Json<Value>, ApiError> {
    let content = b.content.trim();
    if content.is_empty() {
        return Err(bad("empty message"));
    }
    let history = s.db.session_messages(b.session_id).map_err(bad)?;
    let mut messages: Vec<llm::ChatMessage> = history
        .into_iter()
        .map(|m| llm::ChatMessage {
            role: m.role,
            content: m.content,
        })
        .collect();
    messages.push(llm::ChatMessage {
        role: "user".into(),
        content: content.to_string(),
    });
    s.db.add_message(b.session_id, "user", content, None, now())
        .map_err(bad)?;

    let body = ChatBody {
        messages,
        board_outline: b.board_outline,
    };
    match llm::chat(&body).await {
        Ok((text, model)) => {
            s.db.add_message(b.session_id, "assistant", &text, Some(&model), now())
                .map_err(bad)?;
            Ok(Json(json!({ "text": text, "model": model })))
        }
        Err(e) => Err(gateway(e)),
    }
}

async fn list_sessions(
    State(s): State<Arc<AppState>>,
    Query(q): Query<BoardIdQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!(s.db.list_sessions(q.board_id).map_err(bad)?)))
}

#[derive(Deserialize)]
struct CreateSessionBody {
    board_id: i64,
    #[serde(default)]
    title: Option<String>,
}

async fn create_session(
    State(s): State<Arc<AppState>>,
    Json(b): Json<CreateSessionBody>,
) -> Result<Json<Value>, ApiError> {
    let title = b
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Hội thoại mới");
    let id = s.db.create_session(b.board_id, title, now()).map_err(bad)?;
    Ok(Json(json!({ "id": id, "title": title })))
}

#[derive(Deserialize)]
struct RenameSessionBody {
    id: i64,
    title: String,
}

async fn rename_session(
    State(s): State<Arc<AppState>>,
    Json(b): Json<RenameSessionBody>,
) -> Result<Json<Value>, ApiError> {
    let title = if b.title.trim().is_empty() {
        "Hội thoại"
    } else {
        b.title.trim()
    };
    s.db.rename_session(b.id, title).map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

async fn delete_session(
    State(s): State<Arc<AppState>>,
    Json(b): Json<IdBody>,
) -> Result<Json<Value>, ApiError> {
    s.db.delete_session(b.id).map_err(bad)?;
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
struct SessionIdQuery {
    session_id: i64,
}

async fn session_messages(
    State(s): State<Arc<AppState>>,
    Query(q): Query<SessionIdQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!(s
        .db
        .session_messages(q.session_id)
        .map_err(bad)?)))
}

// ---- models ----

async fn models() -> Result<Json<Value>, ApiError> {
    llm::list_models().await.map(Json).map_err(gateway)
}

#[derive(Deserialize)]
struct ModelActiveBody {
    id: String,
}

async fn model_active(Json(b): Json<ModelActiveBody>) -> Result<Json<Value>, ApiError> {
    llm::set_active_model(&b.id).await.map_err(gateway)?;
    Ok(Json(json!({ "success": true, "activeId": b.id })))
}

async fn llm_info() -> Json<Value> {
    let base =
        std::env::var("SENCLAW_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:18788".into());
    let url = format!("{}/api/llm-config", base.trim_end_matches('/'));
    let mut req = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(6));
    // Loopback is not a credential under auth mode `always`.
    if let Some((name, value)) = crate::util::internal_auth::header_for(&url) {
        req = req.header(name, value);
    }
    let fetch = req.send().await;
    match fetch {
        Ok(r) => match r.json::<Value>().await {
            Ok(v) => {
                let active = v.get("activeId").and_then(|x| x.as_str()).unwrap_or("");
                let cfg = v.get("configs").and_then(|a| a.as_array()).and_then(|a| {
                    a.iter()
                        .find(|c| c.get("id").and_then(|x| x.as_str()) == Some(active))
                });
                let model = cfg
                    .and_then(|c| c.get("modelName"))
                    .and_then(|x| x.as_str());
                Json(json!({ "ok": model.is_some(), "daemon": base, "model": model }))
            }
            Err(e) => Json(json!({ "ok": false, "daemon": base, "error": format!("parse: {e}") })),
        },
        Err(e) => Json(
            json!({ "ok": false, "daemon": base, "error": format!("Không kết nối daemon: {e}") }),
        ),
    }
}

/// serde helper: distinguish `"x": null` (clear) from an absent key, for strings.
fn double_option_str<'de, D>(de: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(de)?))
}

fn double_option_i64<'de, D>(de: D) -> Result<Option<Option<i64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<i64>::deserialize(de)?))
}

fn double_option_labels<'de, D>(de: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Option::<Vec<String>>::deserialize(de)?;
    Ok(Some(v.map(|arr| {
        serde_json::to_string(&arr).unwrap_or_else(|_| "[]".into())
    })))
}

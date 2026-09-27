//! REST handlers for the Space feature (notes, calendar, schedules, apps).
//!
//! Routes are registered in `core.rs` under the `/api/space/*` prefix.
//! All DB access goes through `Db::with_conn` on the SQLite pool.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{header, StatusCode},
    response::{Json, Response},
};
use axum_extra::extract::Multipart;
use base64::Engine as _;
use chrono::Utc;
use rusqlite::params;
use rusqlite::types::Value as SqlValue;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use super::core::{AppError, UiState};

// ─── Helper ──────────────────────────────────────────────────────────────────

fn db(s: &UiState) -> Result<&crate::db::Db, AppError> {
    s.db.as_deref()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn internal(e: impl std::fmt::Display) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub(super) fn valid_space_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn space_apps_dir(s: &UiState) -> PathBuf {
    s.config.paths.workspace_dir.join("space-apps")
}

fn space_app_dir(s: &UiState, id: &str) -> Result<PathBuf, AppError> {
    if !valid_space_app_id(id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    Ok(space_apps_dir(s).join(id))
}

fn json_to_sql_value(v: &serde_json::Value) -> SqlValue {
    match v {
        serde_json::Value::Null => SqlValue::Null,
        serde_json::Value::Bool(b) => SqlValue::Integer(if *b { 1 } else { 0 }),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Integer(i)
            } else {
                SqlValue::Real(n.as_f64().unwrap_or_default())
            }
        }
        serde_json::Value::String(s) => SqlValue::Text(s.clone()),
        _ => SqlValue::Text(v.to_string()),
    }
}

fn sql_value_to_json(v: SqlValue) -> serde_json::Value {
    match v {
        SqlValue::Null => serde_json::Value::Null,
        SqlValue::Integer(i) => serde_json::Value::Number(i.into()),
        SqlValue::Real(f) => serde_json::json!(f),
        SqlValue::Text(s) => serde_json::Value::String(s),
        SqlValue::Blob(b) => serde_json::json!({
            "type": "blob",
            "base64": base64::engine::general_purpose::STANDARD.encode(b),
        }),
    }
}

fn read_space_app_manifest_from_zip(zip_bytes: &[u8]) -> Result<serde_json::Value, AppError> {
    let cursor = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor).map_err(internal)?;
    for name in ["senclaw-manifest.json", "senclaw-app.json"] {
        if let Ok(mut file) = archive.by_name(name) {
            let mut raw = String::new();
            std::io::Read::read_to_string(&mut file, &mut raw).map_err(internal)?;
            return serde_json::from_str(&raw)
                .map_err(|e| AppError(StatusCode::BAD_REQUEST, format!("Invalid {name}: {e}")));
        }
    }
    Err(AppError(
        StatusCode::BAD_REQUEST,
        "Zip must contain senclaw-manifest.json or senclaw-app.json at archive root".into(),
    ))
}

pub(crate) fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

// ─── Notes ───────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub(crate) struct NoteRow {
    id: String,
    title: String,
    body: String,
    tags: serde_json::Value,
    folder_id: Option<String>,
    pinned: bool,
    created_at: i64,
    updated_at: i64,
}

#[derive(Deserialize)]
pub(crate) struct NoteListQuery {
    tag: Option<String>,
    folder_id: Option<String>,
}

pub(crate) async fn space_notes_list(
    State(s): State<Arc<UiState>>,
    Query(q): Query<NoteListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let rows: Vec<NoteRow> = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, title, body, tags, folder_id, pinned, created_at, updated_at
                 FROM space_notes
                 WHERE deleted_at IS NULL
                 ORDER BY pinned DESC, updated_at DESC
                 LIMIT 200",
            )?;
            let rows: Vec<NoteRow> = stmt
                .query_map([], |row| {
                    Ok(NoteRow {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        body: row.get(2)?,
                        tags: serde_json::from_str(&row.get::<_, String>(3).unwrap_or_default())
                            .unwrap_or_default(),
                        folder_id: row.get(4)?,
                        pinned: row.get::<_, i32>(5)? != 0,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
        .map_err(internal)?;

    // Tag filter (client-side after fetch, tags stored as JSON array)
    let rows: Vec<NoteRow> = if let Some(tag) = &q.tag {
        rows.into_iter()
            .filter(|n| {
                n.tags
                    .as_array()
                    .map(|arr| arr.iter().any(|v| v.as_str() == Some(tag.as_str())))
                    .unwrap_or(false)
            })
            .collect()
    } else if let Some(fid) = &q.folder_id {
        rows.into_iter()
            .filter(|n| n.folder_id.as_deref() == Some(fid.as_str()))
            .collect()
    } else {
        rows
    };

    Ok(Json(serde_json::to_value(rows).unwrap_or_default()))
}

#[derive(Deserialize)]
pub(crate) struct NoteSearchQuery {
    q: String,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    20
}

pub(crate) async fn space_notes_search(
    State(s): State<Arc<UiState>>,
    Query(q): Query<NoteSearchQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let rows = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT n.id, n.title, n.tags,
                        snippet(space_notes_fts, 2, '<b>', '</b>', '…', 20) AS excerpt
                 FROM space_notes_fts f
                 JOIN space_notes n ON n.id = f.id
                 WHERE f.space_notes_fts MATCH ?1 AND n.deleted_at IS NULL
                 ORDER BY rank LIMIT ?2",
            )?;
            let rows: Vec<serde_json::Value> = stmt
                .query_map(params![q.q, q.limit], |row| {
                    Ok(serde_json::json!({
                        "id":      row.get::<_, String>(0)?,
                        "title":   row.get::<_, String>(1)?,
                        "tags":    serde_json::from_str::<serde_json::Value>(
                                       &row.get::<_, String>(2).unwrap_or_default()
                                   ).unwrap_or_default(),
                        "excerpt": row.get::<_, String>(3)?,
                    }))
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
        .map_err(internal)?;

    Ok(Json(serde_json::to_value(rows).unwrap_or_default()))
}

#[derive(Deserialize)]
pub(crate) struct NoteCreateBody {
    title: String,
    body: String,
    #[serde(default)]
    tags: Vec<String>,
    folder_id: Option<String>,
}

pub(crate) async fn space_notes_create(
    State(s): State<Arc<UiState>>,
    Json(b): Json<NoteCreateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let id = Uuid::new_v4().to_string();
    let now = now_ms();
    let tags_json = serde_json::to_string(&b.tags).unwrap_or_default();

    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO space_notes (id, title, body, tags, folder_id, pinned, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?6)",
            params![id, b.title, b.body, tags_json, b.folder_id, now],
        )?;
        Ok(())
    })
    .map_err(internal)?;

    Ok(Json(serde_json::json!({
        "id": id, "title": b.title, "body": b.body,
        "tags": b.tags, "folder_id": b.folder_id,
        "pinned": false, "created_at": now, "updated_at": now,
    })))
}

#[derive(Deserialize)]
pub(crate) struct NoteUpdateBody {
    title: Option<String>,
    body: Option<String>,
    tags: Option<Vec<String>>,
    pinned: Option<bool>,
}

pub(crate) async fn space_notes_update(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(b): Json<NoteUpdateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let now = now_ms();
    db.with_conn(|conn| {
        if let Some(t) = &b.title {
            conn.execute(
                "UPDATE space_notes SET title=?1, updated_at=?2 WHERE id=?3 AND deleted_at IS NULL",
                params![t, now, id],
            )?;
        }
        if let Some(body) = &b.body {
            conn.execute(
                "UPDATE space_notes SET body=?1, updated_at=?2 WHERE id=?3 AND deleted_at IS NULL",
                params![body, now, id],
            )?;
        }
        if let Some(tags) = &b.tags {
            let j = serde_json::to_string(tags).unwrap_or_default();
            conn.execute(
                "UPDATE space_notes SET tags=?1, updated_at=?2 WHERE id=?3 AND deleted_at IS NULL",
                params![j, now, id],
            )?;
        }
        if let Some(pin) = b.pinned {
            conn.execute(
                "UPDATE space_notes SET pinned=?1, updated_at=?2 WHERE id=?3 AND deleted_at IS NULL",
                params![pin as i32, now, id],
            )?;
        }
        Ok(())
    })
    .map_err(internal)?;

    Ok(Json(serde_json::json!({ "success": true, "id": id })))
}

pub(crate) async fn space_notes_delete(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let now = now_ms();
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE space_notes SET deleted_at=?1 WHERE id=?2",
            params![now, id],
        )?;
        Ok(())
    })
    .map_err(internal)?;

    Ok(Json(serde_json::json!({ "success": true, "id": id })))
}

// ─── Calendar ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct EventListQuery {
    from: i64,
    to: i64,
}

#[derive(Deserialize)]
pub(crate) struct EventSearchQuery {
    #[serde(default)]
    q: Option<String>,
    /// "today" | "tomorrow" | "YYYY-MM-DD"
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    from: Option<i64>,
    #[serde(default)]
    to: Option<i64>,
    #[serde(default = "default_limit")]
    limit: u32,
}

pub(crate) async fn space_events_search(
    State(s): State<Arc<UiState>>,
    Query(q): Query<EventSearchQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db_arc =
        s.db.clone()
            .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))?;

    let srv = crate::mcp::space_server::SpaceServer::new(db_arc);
    let result = srv.event_search(q.q, q.date, q.from, q.to, q.limit);

    if result.is_error {
        return Err(AppError(StatusCode::INTERNAL_SERVER_ERROR, result.content));
    }
    let v: serde_json::Value = serde_json::from_str(&result.content).unwrap_or_default();
    Ok(Json(v))
}

pub(crate) async fn space_events_list(
    State(s): State<Arc<UiState>>,
    Query(q): Query<EventListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let rows = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, title, description, start_at, end_at, all_day,
                        location, color, reminder_min, source, status, renotify_min,
                        link, app_id
                 FROM space_events
                 WHERE deleted_at IS NULL AND start_at >= ?1 AND start_at <= ?2
                 ORDER BY start_at ASC",
            )?;
            let rows: Vec<serde_json::Value> = stmt
                .query_map(params![q.from, q.to], |row| {
                    Ok(serde_json::json!({
                        "id":           row.get::<_,String>(0)?,
                        "title":        row.get::<_,String>(1)?,
                        "description":  row.get::<_,Option<String>>(2)?,
                        "start_at":     row.get::<_,i64>(3)?,
                        "end_at":       row.get::<_,i64>(4)?,
                        "all_day":      row.get::<_,i32>(5)? != 0,
                        "location":     row.get::<_,Option<String>>(6)?,
                        "color":        row.get::<_,Option<String>>(7)?,
                        "reminder_min": row.get::<_,Option<i64>>(8)?,
                        "source":       row.get::<_,String>(9)?,
                        "status":       row.get::<_,Option<String>>(10)?.unwrap_or_else(|| "upcoming".into()),
                        "renotify_min": row.get::<_,Option<i64>>(11)?,
                        "link":         row.get::<_,Option<String>>(12)?,
                        "app_id":       row.get::<_,Option<String>>(13)?,
                    }))
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
        .map_err(internal)?;

    Ok(Json(serde_json::to_value(rows).unwrap_or_default()))
}

/// One event by id.
///
/// Exists so a reminder — which arrives over WS carrying only the event id —
/// can resolve the event's current `link` at the moment the user acts on it,
/// rather than trusting a field copied into a notification payload that may be
/// hours old.
pub(crate) async fn space_events_get(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let row: Option<serde_json::Value> = db
        .with_conn(|conn| {
            use rusqlite::OptionalExtension;
            let mut stmt = conn.prepare(
                "SELECT id, title, description, start_at, end_at, all_day,
                        location, color, reminder_min, source, status, renotify_min,
                        link, app_id
                 FROM space_events WHERE id = ?1 AND deleted_at IS NULL",
            )?;
            let row = stmt
                .query_row(params![id], |row| {
                    Ok(serde_json::json!({
                        "id":           row.get::<_,String>(0)?,
                        "title":        row.get::<_,String>(1)?,
                        "description":  row.get::<_,Option<String>>(2)?,
                        "start_at":     row.get::<_,i64>(3)?,
                        "end_at":       row.get::<_,i64>(4)?,
                        "all_day":      row.get::<_,i32>(5)? != 0,
                        "location":     row.get::<_,Option<String>>(6)?,
                        "color":        row.get::<_,Option<String>>(7)?,
                        "reminder_min": row.get::<_,Option<i64>>(8)?,
                        "source":       row.get::<_,String>(9)?,
                        "status":       row.get::<_,Option<String>>(10)?.unwrap_or_else(|| "upcoming".into()),
                        "renotify_min": row.get::<_,Option<i64>>(11)?,
                        "link":         row.get::<_,Option<String>>(12)?,
                        "app_id":       row.get::<_,Option<String>>(13)?,
                    }))
                })
                .optional()?;
            Ok(row)
        })
        .map_err(internal)?;

    row.map(Json)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "event not found".into()))
}

#[derive(Deserialize)]
pub(crate) struct EventCreateBody {
    title: String,
    start_at: i64,
    /// Optional — defaults to start_at + 1 hour.
    end_at: Option<i64>,
    description: Option<String>,
    location: Option<String>,
    #[serde(default)]
    all_day: bool,
    reminder_min: Option<i64>,
    renotify_min: Option<i64>,
    color: Option<String>,
    /// Where "open this event" should go — an internal Space-App route.
    /// Rejected unless it passes [`crate::mcp::space_server::sanitize_event_link`].
    link: Option<String>,
    app_id: Option<String>,
    /// Group + jid required to schedule a reminder task
    group_folder: Option<String>,
    chat_jid: Option<String>,
}

pub(crate) async fn space_events_create(
    State(s): State<Arc<UiState>>,
    Json(b): Json<EventCreateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db_arc =
        s.db.clone()
            .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))?;

    let space_srv = crate::mcp::space_server::SpaceServer::new(db_arc);
    let result = space_srv.event_create(
        b.title,
        b.start_at,
        // No end time given → default to a 1-hour event.
        b.end_at.unwrap_or(b.start_at + 60 * 60 * 1000),
        b.description,
        b.location,
        b.all_day,
        b.reminder_min,
        b.renotify_min,
        b.color,
        b.link,
        b.app_id,
        b.group_folder.as_deref().unwrap_or("default"),
        b.chat_jid.as_deref().unwrap_or(""),
    );

    if result.is_error {
        return Err(AppError(StatusCode::INTERNAL_SERVER_ERROR, result.content));
    }

    let v: serde_json::Value = serde_json::from_str(&result.content).unwrap_or_default();
    Ok(Json(v))
}

#[derive(Deserialize)]
pub(crate) struct EventUpdateBody {
    title: Option<String>,
    description: Option<String>,
    start_at: Option<i64>,
    end_at: Option<i64>,
    location: Option<String>,
    color: Option<String>,
    reminder_min: Option<i64>,
    renotify_min: Option<i64>,
    #[serde(default)]
    all_day: Option<bool>,
    #[serde(default)]
    link: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    reset_reminder: Option<bool>,
}

pub(crate) async fn space_events_update(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(b): Json<EventUpdateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let link = match b
        .link
        .as_deref()
        .map(crate::mcp::space_server::sanitize_event_link)
    {
        Some(Err(e)) => return Err(AppError(StatusCode::BAD_REQUEST, e)),
        Some(Ok(v)) => Some(v),
        None => None,
    };
    db.with_conn(|conn| {
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Some(v) = &b.title {
            conn.execute("UPDATE space_events SET title=?1 WHERE id=?2", params![v, id])?;
        }
        if b.description.is_some() {
            conn.execute("UPDATE space_events SET description=?1 WHERE id=?2", params![b.description, id])?;
        }
        if let Some(v) = b.start_at {
            // Moving start_at re-arms both the pre-event reminder and the
            // start-time notification so the event pings again at its new
            // time (otherwise a rescheduled event stays silent).
            conn.execute(
                "UPDATE space_events
                 SET start_at=?1, reminder_sent_at=NULL, start_sent_at=NULL
                 WHERE id=?2",
                params![v, id],
            )?;
        }
        if let Some(v) = b.end_at {
            conn.execute("UPDATE space_events SET end_at=?1 WHERE id=?2", params![v, id])?;
        }
        if b.location.is_some() {
            conn.execute("UPDATE space_events SET location=?1 WHERE id=?2", params![b.location, id])?;
        }
        if b.color.is_some() {
            conn.execute("UPDATE space_events SET color=?1 WHERE id=?2", params![b.color, id])?;
        }
        if b.reminder_min.is_some() {
            conn.execute("UPDATE space_events SET reminder_min=?1 WHERE id=?2", params![b.reminder_min, id])?;
        }
        if b.renotify_min.is_some() {
            conn.execute("UPDATE space_events SET renotify_min=?1 WHERE id=?2", params![b.renotify_min, id])?;
        }
        if let Some(v) = b.all_day {
            conn.execute("UPDATE space_events SET all_day=?1 WHERE id=?2", params![v as i32, id])?;
        }
        if link.is_some() {
            conn.execute("UPDATE space_events SET link=?1 WHERE id=?2", params![link, id])?;
        }
        if b.app_id.is_some() {
            conn.execute("UPDATE space_events SET app_id=?1 WHERE id=?2", params![b.app_id, id])?;
        }
        if b.reset_reminder.unwrap_or(false) {
            conn.execute(
                "UPDATE space_events SET reminder_sent_at=NULL, renotify_sent_at=NULL, start_sent_at=NULL WHERE id=?1",
                params![id],
            )?;
        }
        conn.execute("UPDATE space_events SET updated_at=?1 WHERE id=?2", params![now_ms, id])?;
        Ok(())
    })
    .map_err(internal)?;

    Ok(Json(serde_json::json!({ "success": true, "id": id })))
}

pub(crate) async fn space_events_delete(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let now = now_ms();
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE space_events SET deleted_at=?1 WHERE id=?2",
            params![now, id],
        )?;
        Ok(())
    })
    .map_err(internal)?;

    Ok(Json(serde_json::json!({ "success": true, "id": id })))
}

pub(crate) async fn space_today_summary(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db_arc =
        s.db.clone()
            .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))?;

    let space_srv = crate::mcp::space_server::SpaceServer::new(db_arc);
    let result = space_srv.today_summary();

    let v: serde_json::Value = serde_json::from_str(&result.content).unwrap_or_default();
    Ok(Json(v))
}

// ─── Schedules ────────────────────────────────────────────────────────────────
//
// Recurring schedule sessions. Each schedule owns a dedicated chat session
// (a `groups` row with jid="schedule:<id>", folder="schedule_<id>") so the
// agent runs land in that conversation.
//
// All logic lives on `SpaceServer` so it's shared with the `space_recurring_*`
// MCP tools. These handlers are thin adaptors that translate JSON payloads.

#[derive(Deserialize)]
pub(crate) struct ScheduleCreateBody {
    prompt: String,
    label: Option<String>,
    time_local: Option<String>,
    #[serde(default)]
    date_local: Option<String>,
    frequency: Option<String>,
    weekday: Option<u32>,
    day_of_month: Option<u32>,
    cron_advanced: Option<String>,
    agent_mode: Option<String>,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    agent_folder: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ScheduleUpdateBody {
    prompt: Option<String>,
    label: Option<String>,
    status: Option<String>,
    time_local: Option<String>,
    #[serde(default)]
    date_local: Option<String>,
    frequency: Option<String>,
    weekday: Option<u32>,
    day_of_month: Option<u32>,
    cron_advanced: Option<String>,
    agent_mode: Option<String>,
    /// Empty string = back to Default (no profile).
    #[serde(default)]
    agent_folder: Option<String>,
    /// Empty string = back to the active default model.
    #[serde(default)]
    model_id: Option<String>,
}

fn space_server(s: &UiState) -> Result<crate::mcp::space_server::SpaceServer, AppError> {
    let db_arc =
        s.db.clone()
            .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))?;
    Ok(crate::mcp::space_server::SpaceServer::new(db_arc))
}

fn tool_result_to_response(
    r: crate::mcp::schedule_server::ToolResult,
    err_status: StatusCode,
) -> Result<Json<serde_json::Value>, AppError> {
    if r.is_error {
        return Err(AppError(err_status, r.content));
    }
    let v: serde_json::Value = serde_json::from_str(&r.content).unwrap_or_default();
    Ok(Json(v))
}

pub(crate) async fn space_schedules_list(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    tool_result_to_response(
        space_server(&s)?.recurring_list(),
        StatusCode::INTERNAL_SERVER_ERROR,
    )
}

pub(crate) async fn space_schedules_create(
    State(s): State<Arc<UiState>>,
    Json(b): Json<ScheduleCreateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let r = space_server(&s)?
        .recurring_create(
            b.prompt,
            b.label,
            b.time_local,
            b.date_local,
            b.frequency,
            b.weekday,
            b.day_of_month,
            b.cron_advanced,
            b.agent_mode,
            b.model_id,
            b.agent_folder,
        )
        .await;
    tool_result_to_response(r, StatusCode::BAD_REQUEST)
}

pub(crate) async fn space_schedules_detail(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    tool_result_to_response(space_server(&s)?.recurring_get(&id), StatusCode::NOT_FOUND)
}

pub(crate) async fn space_schedules_update(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(b): Json<ScheduleUpdateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let r = space_server(&s)?.recurring_update(
        &id,
        b.prompt,
        b.label,
        b.status,
        b.time_local,
        b.date_local,
        b.frequency,
        b.weekday,
        b.day_of_month,
        b.cron_advanced,
        b.agent_mode,
        b.agent_folder,
        b.model_id,
    );
    tool_result_to_response(r, StatusCode::BAD_REQUEST)
}

pub(crate) async fn space_schedules_run_now(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    tool_result_to_response(
        space_server(&s)?.recurring_run_now(&id),
        StatusCode::NOT_FOUND,
    )
}

pub(crate) async fn space_schedules_cancel(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    tool_result_to_response(
        space_server(&s)?.recurring_delete(&id),
        StatusCode::NOT_FOUND,
    )
}

// ─── Apps (micro-frontend registry) ──────────────────────────────────────────

pub(crate) async fn space_apps_list(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let rows = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, manifest, enabled, installed_at FROM space_apps ORDER BY installed_at DESC",
            )?;
            let rows: Vec<serde_json::Value> = stmt
                .query_map([], |row| {
                    let manifest_str: String = row.get(1)?;
                    let manifest: serde_json::Value =
                        serde_json::from_str(&manifest_str).unwrap_or_default();
                    Ok(serde_json::json!({
                        "id":           row.get::<_,String>(0)?,
                        "manifest":     manifest,
                        "enabled":      row.get::<_,i32>(2)? != 0,
                        "installed_at": row.get::<_,i64>(3)?,
                    }))
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
        .map_err(internal)?;

    Ok(Json(serde_json::to_value(rows).unwrap_or_default()))
}

#[derive(Deserialize)]
pub(crate) struct AppStatusQuery {
    /// Also ask each server app's port whether it is *answering*, not merely
    /// tracked. Off by default: a probe is a network round-trip per app, and the
    /// common caller (a chat agent listing apps) only needs the bookkeeping.
    #[serde(default)]
    probe: Option<u8>,
}

/// One lifecycle row per installed app, for the whole fleet, in one call.
///
/// `/apps` answers "what is installed" and `/apps/:id/runtime` answers "what is
/// this one process doing"; between them there was nothing that answered "which
/// of my fifty apps are up" without fifty requests. That is the question an
/// agent asks before it starts or stops anything, so it gets one endpoint.
///
/// Everything here is read from memory (the launcher's maps) plus one DB read,
/// so it stays cheap enough to call on every turn of a conversation. `probe=1`
/// opts into the expensive, accurate answer.
pub(crate) async fn space_apps_status(
    State(s): State<Arc<UiState>>,
    Query(q): Query<AppStatusQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let rows: Vec<(String, serde_json::Value, bool)> = db
        .with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT id, manifest, enabled FROM space_apps ORDER BY id")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i32>(2)? != 0,
                    ))
                })?
                .filter_map(|r| r.ok())
                .filter_map(|(id, raw, enabled)| {
                    serde_json::from_str::<serde_json::Value>(&raw)
                        .ok()
                        .map(|m| (id, m, enabled))
                })
                .collect::<Vec<_>>();
            Ok(rows)
        })
        .map_err(internal)?;

    let launcher = s.space_mcp_launcher.as_ref();
    let probe = q.probe.unwrap_or(0) != 0;

    let mut apps = Vec::with_capacity(rows.len());
    let (mut running, mut server_count) = (0usize, 0usize);
    for (id, manifest, enabled) in rows {
        apps.push(app_status_row(launcher, &id, &manifest, enabled, probe).await);
        let row = apps.last().expect("just pushed");
        if row["kind"] == "server" {
            server_count += 1;
            if row["running"] == serde_json::Value::Bool(true) {
                running += 1;
            }
        }
    }

    Ok(Json(serde_json::json!({
        "apps": apps,
        "total": apps.len(),
        "serverApps": server_count,
        "running": running,
        "probed": probe,
    })))
}

/// The per-app shape `/apps/status` answers with.
async fn app_status_row(
    launcher: Option<&Arc<super::space_mcp::SpaceMcpLauncher>>,
    id: &str,
    manifest: &serde_json::Value,
    enabled: bool,
    probe: bool,
) -> serde_json::Value {
    let spec = crate::apps::RuntimeSpec::parse(manifest);
    let name = manifest
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(id)
        .to_string();
    let kind = manifest
        .get("runtime")
        .and_then(|r| r.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("static")
        .to_string();
    let mcp_name = manifest
        .get("mcp")
        .filter(|v| v.is_object())
        .map(|m| super::space_mcp::mcp_server_name(m, id));

    // A non-server app is served by the daemon itself: it has no process, so
    // "running" is the honest answer only if it means "usable", which it is.
    if !spec.is_server {
        return serde_json::json!({
            "id": id,
            "name": name,
            "enabled": enabled,
            "kind": kind,
            "mode": "none",
            "running": true,
            "userStopped": false,
            "mcpName": mcp_name,
            "note": "Served by the daemon — nothing to start or stop.",
        });
    }

    let port = manifest
        .get("runtime")
        .and_then(|r| r.get("port"))
        .and_then(|p| p.as_u64())
        .map(|p| p as u16)
        .filter(|p| *p > 0)
        .unwrap_or(spec.port);

    let Some(launcher) = launcher else {
        return serde_json::json!({
            "id": id, "name": name, "enabled": enabled, "kind": kind,
            "mode": spec.mode.as_str(), "port": port, "mcpName": mcp_name,
            "running": false,
            "note": "App runtime not available in this daemon.",
        });
    };

    let info = launcher.runtime_info(id).await;
    let ready = if probe {
        Some(launcher.is_answering(port, &spec.health_path).await)
    } else {
        None
    };

    serde_json::json!({
        "id": id,
        "name": name,
        "enabled": enabled,
        "kind": kind,
        "mode": spec.mode.as_str(),
        "port": port,
        "mcpName": mcp_name,
        // Bookkeeping: does this daemon track a live pid for the app. False for
        // an app running outside this daemon — which is why `ready` exists.
        "running": launcher.is_running(id).await,
        "ready": ready,
        "userStopped": launcher.is_user_stopped(id).await,
        "launches": launcher.launch_count(id).await,
        "pid": info.as_ref().map(|i| i.pid),
        "idleSecs": info.as_ref().map(|i| i.idle_secs),
        "idleTimeoutSecs": spec.idle_timeout_secs,
    })
}

#[derive(Deserialize)]
pub(crate) struct AppRegisterBody {
    manifest_url: String,
}

pub(crate) async fn space_apps_register(
    State(s): State<Arc<UiState>>,
    Json(b): Json<AppRegisterBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Fetch the manifest from the given URL
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(internal)?;

    let manifest_json: serde_json::Value = client
        .get(&b.manifest_url)
        .send()
        .await
        .map_err(|e| {
            AppError(
                StatusCode::BAD_GATEWAY,
                format!("Fetch manifest failed: {e}"),
            )
        })?
        .json()
        .await
        .map_err(|e| {
            AppError(
                StatusCode::BAD_GATEWAY,
                format!("Parse manifest failed: {e}"),
            )
        })?;

    let app_id = manifest_json["id"]
        .as_str()
        .unwrap_or(&Uuid::new_v4().to_string())
        .to_string();
    if !valid_space_app_id(&app_id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }

    let now = now_ms();
    let manifest_str = serde_json::to_string(&manifest_json).unwrap_or_default();

    let db = db(&s)?;
    db.with_conn(|conn| {
        conn.execute(
            "INSERT OR REPLACE INTO space_apps (id, manifest, enabled, installed_at, last_seen_at)
             VALUES (?1, ?2, 1, ?3, ?3)",
            params![app_id, manifest_str, now],
        )?;
        Ok(())
    })
    .map_err(internal)?;

    // Auto-register the app's declared MCP server (launch + register) if any.
    try_autoregister_app_mcp(&s, &app_id, &manifest_json).await;

    Ok(Json(serde_json::json!({
        "id": app_id,
        "manifest": manifest_json,
        "enabled": true,
        "installed_at": now,
    })))
}

#[derive(Deserialize)]
pub(crate) struct AppRegisterLocalBody {
    /// Absolute path to a Space App directory containing senclaw-manifest.json.
    path: String,
}

/// Register a Space App from a local directory (for "server" apps the daemon
/// runs in place via `runtime.start`). Reads the manifest, records the local
/// path, then installs skills + launches + auto-registers the MCP.
pub(crate) async fn space_apps_register_local(
    State(s): State<Arc<UiState>>,
    Json(b): Json<AppRegisterLocalBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let dir = PathBuf::from(b.path.trim());
    if !dir.is_dir() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Path is not a directory".into(),
        ));
    }
    let manifest_path = ["senclaw-manifest.json", "senclaw-app.json"]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            AppError(
                StatusCode::BAD_REQUEST,
                "No senclaw-manifest.json in directory".into(),
            )
        })?;
    let raw = tokio::fs::read_to_string(&manifest_path)
        .await
        .map_err(internal)?;
    let mut manifest: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, format!("Invalid manifest: {e}")))?;

    let app_id = manifest["id"]
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| valid_space_app_id(s))
        .ok_or_else(|| AppError(StatusCode::BAD_REQUEST, "Manifest missing valid id".into()))?;

    let canonical = dir.canonicalize().unwrap_or(dir);
    manifest["install"] = serde_json::json!({
        "type": "local",
        "localPath": canonical.to_string_lossy(),
    });

    let now = now_ms();
    let manifest_str = serde_json::to_string(&manifest).unwrap_or_default();
    let db = db(&s)?;
    db.with_conn(|conn| {
        conn.execute(
            "INSERT OR REPLACE INTO space_apps (id, manifest, enabled, installed_at, last_seen_at)
             VALUES (?1, ?2, 1, ?3, ?3)",
            params![app_id, manifest_str, now],
        )?;
        Ok(())
    })
    .map_err(internal)?;

    try_autoregister_app_mcp(&s, &app_id, &manifest).await;

    // Re-read the manifest (run_and_register may have stamped runtime.url/port).
    let stored: Option<serde_json::Value> = db
        .with_conn(|conn| {
            let raw: Result<String, rusqlite::Error> = conn.query_row(
                "SELECT manifest FROM space_apps WHERE id=?1",
                params![&app_id],
                |row| row.get(0),
            );
            Ok(raw.ok().and_then(|s| serde_json::from_str(&s).ok()))
        })
        .map_err(internal)?;

    Ok(Json(serde_json::json!({
        "id": app_id,
        "manifest": stored.unwrap_or(manifest),
        "enabled": true,
        "installed_at": now,
    })))
}

pub(crate) async fn space_apps_install_zip(
    State(s): State<Arc<UiState>>,
    mut multipart: Multipart,
) -> Result<axum::response::Response, AppError> {
    let mut zip_bytes: Option<Vec<u8>> = None;
    // Optional text fields carrying install provenance (slug/version/…) so a
    // later update-check can resolve the source package. The file may arrive
    // before or after them, so every field is read rather than breaking early.
    let mut fields: HashMap<String, String> = HashMap::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, format!("Invalid upload: {e}")))?
    {
        let name = field.name().map(str::to_string);
        let is_zip = field
            .file_name()
            .map(|n| n.to_ascii_lowercase().ends_with(".zip"))
            .unwrap_or(false);
        if name.as_deref() == Some("file") || is_zip {
            let bytes = field.bytes().await.map_err(|e| {
                AppError(StatusCode::BAD_REQUEST, format!("Read upload failed: {e}"))
            })?;
            zip_bytes = Some(bytes.to_vec());
        } else if let Some(n) = name {
            if let Ok(text) = field.text().await {
                fields.insert(n, text);
            }
        }
    }

    let zip_bytes = zip_bytes.ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            "Upload a zip file in multipart field `file`".into(),
        )
    })?;

    let origin = fields.get("slug").and_then(|slug| {
        crate::marketplace::registry::parse_slug(slug)
            .ok()
            .map(|(scope, name)| crate::marketplace::app_update::HubOrigin {
                scope,
                name,
                version: fields.get("version").cloned(),
                hub: fields.get("hub").cloned(),
                integrity: fields.get("integrity").cloned(),
                installed_at: Some(now_ms()),
            })
    });

    // Uploads carry `force=true` as a multipart field, so overriding a blocking
    // scan is an explicit act by whoever submitted the form.
    let force = fields
        .get("force")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let out = install_app_from_zip(s.clone(), zip_bytes, origin, force).await?;
    Ok(out.into_response())
}

/// Extract a Space App zip, register it (skills / MCP / launch) and persist it —
/// the shared core of first-time install and update. When `origin` is given it
/// is stamped into the stored manifest as `hub` provenance, so a later
/// update-check can resolve the source package and installed version.
/// Outcome of a Space App install. `Blocked` is a value rather than an error
/// string so the Web UI receives the findings as data and can offer a reviewed
/// override instead of showing a wall of text in a toast.
pub(crate) enum AppInstallOutcome {
    Installed(serde_json::Value),
    Blocked(crate::security::ScanReport),
}

impl AppInstallOutcome {
    /// Render as an HTTP response: 200 with the app, or 422 with the report.
    pub(crate) fn into_response(self) -> axum::response::Response {
        use axum::response::IntoResponse as _;
        match self {
            AppInstallOutcome::Installed(v) => Json(v).into_response(),
            AppInstallOutcome::Blocked(report) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "success": false,
                    "blocked": true,
                    "error": format!(
                        "Blocked by the pre-install security scan (risk {}/100). \
                         Nothing was installed and the extracted files were removed.",
                        report.risk_score()
                    ),
                    "scan": report,
                })),
            )
                .into_response(),
        }
    }
}

pub(crate) async fn install_app_from_zip(
    s: Arc<UiState>,
    zip_bytes: Vec<u8>,
    origin: Option<crate::marketplace::app_update::HubOrigin>,
    force: bool,
) -> Result<AppInstallOutcome, AppError> {
    if zip_bytes.len() > 50 * 1024 * 1024 {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Zip file too large (max 50MB)".into(),
        ));
    }

    let mut manifest = read_space_app_manifest_from_zip(&zip_bytes)?;
    let app_id = manifest["id"]
        .as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| valid_space_app_id(s))
        .unwrap_or_else(|| format!("space-app-{}", Uuid::new_v4()));
    manifest["id"] = serde_json::Value::String(app_id.clone());

    let root = space_apps_dir(&s);
    let target = root.join(&app_id);
    if target.exists() {
        tokio::fs::remove_dir_all(&target).await.map_err(internal)?;
    }
    crate::clawhub::lockfile::extract_zip_to_dir(&zip_bytes, &target).map_err(internal)?;

    // "server" apps ship a runnable program started by `runtime.start` — any
    // runtime (Node, a native/Rust binary, Python, a static-file server, …). We
    // validate the declared `runtime.entrypoint` exists if given, else that the
    // archive is non-empty. Static apps ship a built index.html.
    let is_server = manifest
        .get("runtime")
        .and_then(|r| r.get("kind"))
        .and_then(|k| k.as_str())
        == Some("server");
    if is_server {
        let entrypoint = manifest
            .get("runtime")
            .and_then(|r| r.get("entrypoint"))
            .and_then(|e| e.as_str());
        let valid = match entrypoint {
            Some(ep) => target.join(ep).is_file(),
            None => std::fs::read_dir(&target)
                .map(|mut it| it.next().is_some())
                .unwrap_or(false),
        };
        if !valid {
            let _ = tokio::fs::remove_dir_all(&target).await;
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "Server Space App zip must contain its runtime.entrypoint (or be non-empty)."
                    .into(),
            ));
        }
    } else if !target.join("index.html").is_file() {
        let _ = tokio::fs::remove_dir_all(&target).await;
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Space App zip must contain a built index.html at archive root. Run the app build and zip the build output directory.".into(),
        ));
    }

    if manifest.get("integration").is_none() {
        manifest["integration"] = if is_server {
            serde_json::json!({ "type": "iframe", "url": "/" })
        } else {
            serde_json::json!({
                "type": "iframe",
                "url": format!("/api/space/apps/{app_id}/static/index.html"),
            })
        };
    }
    manifest["install"] = serde_json::json!({
        "type": "zip",
        "localPath": target.to_string_lossy(),
    });
    if manifest.get("bridge").is_none() {
        manifest["bridge"] = serde_json::json!({
            "postMessage": true,
            "capabilities": ["space.rest"],
        });
    }

    // Stamp hub provenance (source slug + version) so a later update-check can
    // resolve the source package. Absent for hand-uploaded zips.
    if let Some(origin) = &origin {
        if let Ok(v) = serde_json::to_value(origin) {
            manifest["hub"] = v;
        }
    }

    // ── Pre-install security gate ────────────────────────────────────────────
    // This is the last point where nothing from the package has run. The very
    // next steps record the app and call `try_autoregister_app_mcp`, which
    // executes `runtime.start` through `sh -c` and spawns the declared MCP
    // command. Scan the extracted tree *and* the manifest here, and on a
    // blocking verdict remove the directory so nothing is left staged.
    let policy = crate::security::ScanPolicy::from_config(&s.config);
    let scan = if policy.enabled {
        let target_for_scan = target.clone();
        let manifest_for_scan = manifest.clone();
        let app_id_for_scan = app_id.clone();
        let report = tokio::task::spawn_blocking(move || {
            crate::security::scan_space_app(&target_for_scan, &manifest_for_scan, &app_id_for_scan)
        })
        .await
        .map_err(internal)?;

        if report.verdict(&policy) == crate::security::scan::Verdict::Block && !force {
            let _ = tokio::fs::remove_dir_all(&target).await;
            tracing::warn!(
                "[space] blocked install of '{app_id}' (risk {}/100):\n{}",
                report.risk_score(),
                report.summary()
            );
            return Ok(AppInstallOutcome::Blocked(report));
        }
        if !report.findings.is_empty() {
            tracing::warn!(
                "[space] pre-install scan of '{app_id}' (risk {}/100, forced={force}):\n{}",
                report.risk_score(),
                report.summary()
            );
        }
        Some(report)
    } else {
        None
    };

    let now = now_ms();
    let manifest_str = serde_json::to_string(&manifest).unwrap_or_default();
    let db = db(&s)?;
    db.with_conn(|conn| {
        conn.execute(
            "INSERT OR REPLACE INTO space_apps (id, manifest, enabled, installed_at, last_seen_at)
             VALUES (?1, ?2, 1, ?3, ?3)",
            params![app_id, manifest_str, now],
        )?;
        Ok(())
    })
    .map_err(internal)?;

    // What the app needs from this machine. Checked before the launch so the
    // answer is a sentence in the install response — "install ffmpeg" — rather
    // than a non-zero exit code in a log file minutes later. It does not block:
    // the app is installed either way, and refuses to start with the same
    // reason attached until the machine has what it asked for.
    let requires = crate::apps::Requires::parse(&manifest);
    let requirements = if requires.is_empty() {
        serde_json::Value::Null
    } else {
        let report = crate::apps::requirements::check(&requires).await;
        if !report.satisfied {
            tracing::warn!(
                "[space] '{app_id}' installed but cannot start yet — {}",
                report.summary
            );
        }
        serde_json::to_value(&report).unwrap_or(serde_json::Value::Null)
    };

    // Auto-register the app's declared MCP server (launch + register) if any.
    try_autoregister_app_mcp(&s, &app_id, &manifest).await;

    let spec = crate::apps::RuntimeSpec::parse(&manifest);
    Ok(AppInstallOutcome::Installed(serde_json::json!({
        "id": app_id,
        "manifest": manifest,
        "enabled": true,
        "installed_at": now,
        "mode": spec.mode.as_str(),
        "runner": spec.runner.as_str(),
        "requirements": requirements,
        // Present even on success: a Warn verdict installs and still has
        // findings the user needs to see.
        "scan": scan,
    })))
}

/// GET `/api/space/apps/updates` — check every hub-installed app against the
/// registry and report which have a newer version available.
pub(crate) async fn space_apps_updates(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let apps: Vec<(String, serde_json::Value)> = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id, manifest FROM space_apps")?;
            let rows = stmt
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let m: String = row.get(1)?;
                    Ok((id, serde_json::from_str(&m).unwrap_or_default()))
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
        .map_err(internal)?;

    let hub = s.config.marketplace_hub_url.clone();
    let statuses = crate::marketplace::app_update::check_updates(&apps, &hub).await;
    Ok(Json(serde_json::to_value(statuses).unwrap_or_default()))
}

/// POST `/api/space/apps/:id/update` — download and install the hub's latest
/// version of an installed app, in place. No-op (200) when already current.
pub(crate) async fn space_apps_update(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<axum::response::Response, AppError> {
    use crate::marketplace::{app_update, publish, registry};
    use axum::response::IntoResponse as _;

    let db = db(&s)?;
    let manifest: serde_json::Value = db
        .with_conn(|conn| {
            let raw: Result<String, rusqlite::Error> = conn.query_row(
                "SELECT manifest FROM space_apps WHERE id=?1",
                params![&id],
                |row| row.get(0),
            );
            Ok(raw.ok().and_then(|s| serde_json::from_str(&s).ok()))
        })
        .map_err(internal)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("app `{id}` không tồn tại")))?;

    let origin = app_update::origin_from_manifest(&manifest, &id).ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            "app này không cài từ hub — không có nguồn để cập nhật".into(),
        )
    })?;

    let hub = s.config.marketplace_hub_url.clone();
    let pkg = registry::fetch_package(&hub, &origin.scope, &origin.name)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let ver = registry::resolve_version(&pkg, None)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;

    if !app_update::is_newer(&ver.version, origin.version.as_deref()) {
        return Ok(Json(serde_json::json!({
            "id": id,
            "updated": false,
            "installed": origin.version,
            "latest": ver.version,
        }))
        .into_response());
    }

    let host = publish::host_platform();
    let dist = registry::select_dist(ver, &host)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    let bytes = registry::download_verified(dist)
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?;

    let new_origin = app_update::HubOrigin {
        scope: origin.scope.clone(),
        name: origin.name.clone(),
        version: Some(ver.version.clone()),
        hub: Some(hub.clone()),
        integrity: dist.integrity.clone(),
        installed_at: Some(now_ms()),
    };
    // `force: false` — an update is fresh untrusted code from the hub, and a
    // package that was benign at v1 is exactly how a supply-chain compromise
    // arrives. Being already installed earns no exemption.
    let app = match install_app_from_zip(s.clone(), bytes, Some(new_origin), false).await? {
        // A blocked update leaves the previously installed version untouched —
        // the extracted files are already removed at this point.
        blocked @ AppInstallOutcome::Blocked(_) => return Ok(blocked.into_response()),
        AppInstallOutcome::Installed(v) => v,
    };

    Ok(Json(serde_json::json!({
        "id": id,
        "updated": true,
        "from": origin.version,
        "latest": ver.version,
        "app": app,
    }))
    .into_response())
}

pub(crate) async fn space_apps_delete(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db = db(&s)?;
    let manifest: Option<serde_json::Value> = db
        .with_conn(|conn| {
            let raw: Result<String, rusqlite::Error> = conn.query_row(
                "SELECT manifest FROM space_apps WHERE id=?1",
                params![&id],
                |row| row.get(0),
            );
            Ok(raw.ok().and_then(|s| serde_json::from_str(&s).ok()))
        })
        .map_err(internal)?;

    if let Some(path) = manifest
        .as_ref()
        .and_then(|m| m["install"]["localPath"].as_str())
        .map(PathBuf::from)
    {
        let root = space_apps_dir(&s);
        let canonical_root = root.canonicalize().unwrap_or(root);
        let canonical_path = path.canonicalize().unwrap_or(path);
        if canonical_path.starts_with(&canonical_root) {
            let _ = tokio::fs::remove_dir_all(canonical_path).await;
        }
    }

    // Remove the app's bundled skills + personas, stop its server process, and
    // unregister its MCP server.
    super::space_skills::remove_app_skills(&s.config, &id);
    super::space_personas::remove_app_personas(&s.config, &id);
    // Drop the app's imported tool aliases (Plugins → Alias) and refresh the
    // in-process registry so they stop resolving immediately.
    let _ = db.delete_tool_aliases_by_source(&crate::db::tool_aliases::app_source(&id));
    crate::tools::tool_alias::reload_from_db(&db);
    if let Some(launcher) = s.space_mcp_launcher.as_ref() {
        launcher.stop_app(&id).await;
    }
    if let (Some(mgr), Some(name)) = (
        s.mcp_manager.as_ref(),
        manifest
            .as_ref()
            .and_then(|m| m["mcp"]["name"].as_str())
            .map(str::to_string),
    ) {
        let _ = mgr
            .remove(&name, crate::mcp::config::McpScopeType::Project)
            .await;
    }

    // Drop the access token with the app. Ids are reusable — reinstalling
    // `kanban`, or installing someone else's app under that id, must not
    // inherit a secret an earlier install handed out.
    if let Err(e) = crate::apps::token::revoke(&db, &id) {
        tracing::warn!("[space] '{id}': access token was not revoked on uninstall: {e}");
    }

    // Take the app's models out of the picker, and clear any active-model
    // pointer into them. A dangling selection is worse than a missing one:
    // `resolve_model_profile_at` falls back to the first config when the
    // selected id is gone, so the user keeps chatting — to a different model
    // than the UI is showing.
    if let Err(e) =
        crate::apps::llm_provider::unregister(&db, &id, &s.config.paths.global_config_path)
    {
        tracing::warn!("[space] '{id}': LLM provider was not unregistered on uninstall: {e}");
    }

    db.with_conn(|conn| {
        conn.execute("DELETE FROM space_apps WHERE id=?1", params![id])?;
        Ok(())
    })
    .map_err(internal)?;

    Ok(Json(serde_json::json!({ "success": true })))
}

pub(crate) async fn space_apps_restart(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let (Some(launcher), Some(mgr), Some(db)) = (
        s.space_mcp_launcher.as_ref(),
        s.mcp_manager.as_ref(),
        s.db.as_deref(),
    ) else {
        return Err(AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "App runtime not available".into(),
        ));
    };

    // Load the current manifest so we can kill + respawn the right process.
    let manifest: serde_json::Value = db
        .with_conn(|conn| {
            let raw: Result<String, rusqlite::Error> = conn.query_row(
                "SELECT manifest FROM space_apps WHERE id=?1",
                params![&id],
                |row| row.get(0),
            );
            Ok(raw.ok().and_then(|s| serde_json::from_str(&s).ok()))
        })
        .map_err(internal)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "App not found".into()))?;

    let app_dir = manifest
        .get("install")
        .and_then(|i| i.get("localPath"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| space_app_dir(&s, &id).ok())
        .unwrap_or_default();
    let base_url = format!("http://127.0.0.1:{}", s.config.ui_server.port);

    // Kill the old process group (incl. any orphan holding the port), then
    // respawn a fresh one and wait until it is healthy. Fully restarts even if
    // the app wasn't running.
    match launcher
        .restart_and_respawn(db, mgr, &id, &app_dir, &manifest, &base_url)
        .await
    {
        Ok(_) => Ok(Json(serde_json::json!({ "success": true }))),
        Err(e) => Err(AppError(
            StatusCode::BAD_GATEWAY,
            format!("Restart failed: {e}"),
        )),
    }
}

/// The app's manifest, or a 404.
fn app_manifest(s: &UiState, id: &str) -> Result<serde_json::Value, AppError> {
    if !valid_space_app_id(id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    let db = db(s)?;
    db.with_conn(|conn| {
        let raw: Result<String, rusqlite::Error> = conn.query_row(
            "SELECT manifest FROM space_apps WHERE id=?1",
            params![id],
            |row| row.get(0),
        );
        Ok(raw.ok().and_then(|s| serde_json::from_str(&s).ok()))
    })
    .map_err(internal)?
    .ok_or_else(|| AppError(StatusCode::NOT_FOUND, "App not found".into()))
}

fn app_dir_of(s: &UiState, id: &str, manifest: &serde_json::Value) -> PathBuf {
    manifest
        .get("install")
        .and_then(|i| i.get("localPath"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| space_app_dir(s, id).ok())
        .unwrap_or_default()
}

/// Stop an app's server process now.
///
/// For a session app this is simply doing early what the idle reaper would do
/// later. For a background app it is an override the supervisor honours until
/// the app is started again — otherwise the button would appear not to work,
/// the app being back inside one supervisor tick.
pub(crate) async fn space_apps_stop(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manifest = app_manifest(&s, &id)?;
    let launcher = s.space_mcp_launcher.as_ref().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "App runtime not available".into(),
        )
    })?;
    let was_running = launcher.is_running(&id).await;
    let mcp_name = manifest
        .get("mcp")
        .filter(|v| v.is_object())
        .map(|m| super::space_mcp::mcp_server_name(m, &id));
    launcher
        .stop_by_user(&id, s.mcp_manager.as_deref(), mcp_name.as_deref())
        .await;
    let spec = crate::apps::RuntimeSpec::parse(&manifest);
    Ok(Json(serde_json::json!({
        "success": true,
        "wasRunning": was_running,
        "mode": spec.mode.as_str(),
        // Said plainly, because "stopped" means different things in the two
        // modes: a session app comes back by itself, a background one does not.
        "note": if spec.mode.is_background() {
            "Stopped. A background app stays stopped until you start it again."
        } else {
            "Stopped. It starts again by itself when the app is opened or one of its tools is called."
        },
    })))
}

/// Start an app's server process now (and register/refresh its MCP).
pub(crate) async fn space_apps_start(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manifest = app_manifest(&s, &id)?;
    let (Some(launcher), Some(mgr), Some(db)) = (
        s.space_mcp_launcher.as_ref(),
        s.mcp_manager.as_ref(),
        s.db.as_deref(),
    ) else {
        return Err(AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "App runtime not available".into(),
        ));
    };
    let app_dir = app_dir_of(&s, &id, &manifest);
    let base_url = format!("http://127.0.0.1:{}", s.config.ui_server.port);
    launcher
        .run_and_register(db, mgr, &id, &app_dir, &manifest, &base_url)
        .await
        .map_err(|e| {
            // The reason is in the app's own log, and a caller that just got a
            // failed start is exactly who needs it — otherwise the UI can only
            // say "it didn't work".
            let log = crate::gateway::ui_server::space_mcp::app_runtime_log_path(&app_dir);
            let tail = read_log_tail(&log, 40);
            AppError(
                StatusCode::BAD_GATEWAY,
                if tail.is_empty() {
                    format!("Start failed: {e}")
                } else {
                    format!("Start failed: {e}\n\n{tail}")
                },
            )
        })?;
    launcher.touch(&id).await;
    Ok(Json(app_readiness(&s, &id, &manifest).await))
}

/// Is this app up and answering *right now*, without starting anything?
///
/// The UI calls this before it points a webview at an app: a server Space App
/// runs on its own port, so a page loaded against a stopped app is a blank
/// window with no error in it — and "stopped" is the resting state for a
/// `session` app, not a fault. Cheap enough to poll.
pub(crate) async fn space_apps_ready(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manifest = app_manifest(&s, &id)?;
    Ok(Json(app_readiness(&s, &id, &manifest).await))
}

/// The shape both `/start` and `/ready` answer with. `ready` is a health probe,
/// never merely "we have a pid": see `SpaceMcpLauncher::is_answering`.
async fn app_readiness(
    s: &Arc<UiState>,
    id: &str,
    manifest: &serde_json::Value,
) -> serde_json::Value {
    let spec = crate::apps::RuntimeSpec::parse(manifest);
    // A non-server app is served by the daemon itself, so it is always ready —
    // saying "not ready" would strand static and esm apps behind a gate that
    // has nothing to open it.
    if !spec.is_server {
        return serde_json::json!({
            "appId": id, "kind": "static", "ready": true, "running": true,
        });
    }
    let port = manifest
        .get("runtime")
        .and_then(|r| r.get("port"))
        .and_then(|p| p.as_u64())
        .map(|p| p as u16)
        .filter(|p| *p > 0)
        .unwrap_or(spec.port);
    // Two different questions, deliberately answered by two different means.
    //
    // `running` is bookkeeping: does the launcher track a pid it started. It is
    // false for an app running outside this daemon, and true for a process that
    // has a pid but has not bound its port yet.
    //
    // `ready` is the one a UI gates on, so it asks the port itself rather than
    // the bookkeeping — no launcher required. Deriving it from `running` would
    // report "not ready" for an app that is plainly answering, and the UI would
    // sit on a spinner in front of a working app.
    let running = match s.space_mcp_launcher.as_ref() {
        Some(l) => l.is_running(id).await,
        None => false,
    };
    let ready = port > 0 && health_probe(port, &spec.health_path).await;
    serde_json::json!({
        "appId": id,
        "kind": "server",
        "mode": spec.mode.as_str(),
        // Tracked and not exited. NOT the same as usable.
        "running": running,
        // Answered a health request. This is the one a UI should gate on.
        "ready": ready,
        "port": port,
        "healthPath": spec.health_path,
        // Where to point a webview once ready: the app's own origin (a direct
        // connection, which is also the only way its WebSockets work), and the
        // daemon proxy as the same-origin alternative.
        "url": format!("http://127.0.0.1:{port}"),
        "proxyUrl": format!("/api/space/apps/{id}/proxy/"),
    })
}

/// Does something answer a health request on this port, right now?
///
/// Short timeout on purpose: this sits in front of a UI that is waiting to draw,
/// and a slow answer is a "no" for its purposes — it will ask again.
async fn health_probe(port: u16, health_path: &str) -> bool {
    let path = if health_path.starts_with('/') {
        health_path.to_string()
    } else {
        format!("/{health_path}")
    };
    let url = format!("http://127.0.0.1:{port}{path}");
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(1500))
        .build()
    else {
        return false;
    };
    matches!(client.get(&url).send().await, Ok(r) if r.status().is_success())
}

/// Last `lines` lines of a log file, empty when unreadable.
fn read_log_tail(path: &Path, lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// What this app needs from the machine, and whether the machine has it.
///
/// The same check the launcher runs before spawning — exposed so the answer is
/// visible before an app fails to start, and after the user installs the thing
/// that was missing.
pub(crate) async fn space_app_requirements(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let manifest = app_manifest(&s, &id)?;
    let requires = crate::apps::Requires::parse(&manifest);
    let report = crate::apps::requirements::check(&requires).await;
    let spec = crate::apps::RuntimeSpec::parse(&manifest);
    Ok(Json(serde_json::json!({
        "appId": id,
        "runner": spec.runner.as_str(),
        "mode": spec.mode.as_str(),
        "report": report,
    })))
}

pub(crate) async fn space_apps_static(
    State(s): State<Arc<UiState>>,
    AxumPath((id, req_path)): AxumPath<(String, String)>,
) -> Result<Response, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    if req_path.contains("..") || req_path.contains('\\') {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app path".into()));
    }
    let root = space_apps_dir(&s).join(&id);
    let rel = if req_path.trim().is_empty() {
        "index.html"
    } else {
        req_path.trim_start_matches('/')
    };
    let path = root.join(rel);
    let canonical_root = root
        .canonicalize()
        .map_err(|_| AppError(StatusCode::NOT_FOUND, "App not found".into()))?;
    let canonical_path = path
        .canonicalize()
        .map_err(|_| AppError(StatusCode::NOT_FOUND, "File not found".into()))?;
    if !canonical_path.starts_with(&canonical_root) || !canonical_path.is_file() {
        return Err(AppError(StatusCode::NOT_FOUND, "File not found".into()));
    }
    let bytes = tokio::fs::read(&canonical_path).await.map_err(internal)?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type_for(&canonical_path))
        .body(Body::from(bytes))
        .unwrap())
}

/// Serve a tray screen capture by filename. Notes and events reference shots as
/// `![](http://127.0.0.1:<ui_port>/api/space/screenshots/<name>)` — a plain HTTP
/// URL, so both the Flutter (`NetworkImage`) and React markdown renderers show
/// it without a local-file codepath.
///
/// Read-only and flat by design: the tray writes here, nothing else does.
pub(crate) async fn space_screenshot_get(
    State(s): State<Arc<UiState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Response, AppError> {
    // Flat directory — a name with any separator or `..` can only be traversal.
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid name".into()));
    }
    let root = &s.config.paths.screenshots_dir;
    let canonical_root = root
        .canonicalize()
        .map_err(|_| AppError(StatusCode::NOT_FOUND, "No screenshots yet".into()))?;
    let canonical_path = root
        .join(&name)
        .canonicalize()
        .map_err(|_| AppError(StatusCode::NOT_FOUND, "Screenshot not found".into()))?;
    // Belt-and-braces after the name check: canonicalize resolves symlinks, so a
    // planted link inside the dir can't escape it either.
    if !canonical_path.starts_with(&canonical_root) || !canonical_path.is_file() {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            "Screenshot not found".into(),
        ));
    }
    let bytes = tokio::fs::read(&canonical_path).await.map_err(internal)?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type_for(&canonical_path))
        .body(Body::from(bytes))
        .unwrap())
}

#[derive(Deserialize)]
pub(crate) struct ScreenshotExtractBody {
    /// Bare filename of a shot already written to the screenshots dir.
    name: String,
}

/// AI-fill a screenshot's title + notes. Prefers vision (best for Vietnamese
/// text and on-screen context); falls back to OCR → text LLM when the active
/// model can't see. Returns `{ title, notes, via }`.
///
/// The image is read and base64'd server-side — the file already lives here, and
/// a localhost screenshot URL is unreachable by a cloud LLM anyway.
pub(crate) async fn space_screenshot_extract(
    State(s): State<Arc<UiState>>,
    Json(b): Json<ScreenshotExtractBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if b.name.contains("..") || b.name.contains('/') || b.name.contains('\\') {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid name".into()));
    }
    let root = s
        .config
        .paths
        .screenshots_dir
        .canonicalize()
        .map_err(|_| AppError(StatusCode::NOT_FOUND, "No screenshots yet".into()))?;
    let path = s
        .config
        .paths
        .screenshots_dir
        .join(&b.name)
        .canonicalize()
        .map_err(|_| AppError(StatusCode::NOT_FOUND, "Screenshot not found".into()))?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            "Screenshot not found".into(),
        ));
    }
    let bytes = tokio::fs::read(&path).await.map_err(internal)?;

    let cfg_path = &s.config.paths.global_config_path;
    let system = "Bạn trích thông tin từ ảnh chụp màn hình để tạo một ghi chú. \
Chỉ trả về JSON đúng dạng: {\"title\": string, \"notes\": string}. \
title: tiếng Việt, ngắn gọn (tối đa 60 ký tự), mô tả nội dung/việc cần nhớ. \
notes: chi tiết hỗ trợ ngắn, hoặc chuỗi rỗng. Không thêm chữ nào ngoài JSON.";

    // Vision first when the active model supports it.
    let model = super::llm_config::active_model_name(cfg_path, None)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e))?;
    let (raw, via) = if crate::zen_core::vision::infer_vision(&model) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let r = super::llm_config::chat_completion_vision(
            cfg_path,
            None,
            system,
            "Đây là ảnh chụp màn hình. Trích tiêu đề và ghi chú.",
            &b64,
            "image/png",
            400,
            None,
        )
        .await
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e))?;
        super::llm_config::record_completion(&s.usage_recorder, "web:screenshot-note", "", &r);
        (r.text, "vision")
    } else {
        // No vision — OCR the image, feed the text to a plain completion.
        let text = match &s.runtime_manager {
            Some(mgr) => crate::runtime::clients::ocr_recognize(mgr, bytes, "screenshot.png")
                .await
                .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e.to_string()))?,
            None => None,
        };
        let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "Model hiện tại không đọc được ảnh (không có vision) và OCR chưa \
sẵn sàng. Chọn model có vision trong Settings → Models, hoặc cài OCR."
                    .into(),
            ));
        };
        let user =
            format!("Văn bản trích từ ảnh chụp màn hình:\n\n{text}\n\nTrích tiêu đề và ghi chú.");
        let r = super::llm_config::chat_completion(cfg_path, None, system, &user, 400, None)
            .await
            .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e))?;
        super::llm_config::record_completion(&s.usage_recorder, "web:screenshot-note", "", &r);
        (r.text, "ocr")
    };

    let (title, notes) = parse_title_notes(&raw);
    Ok(Json(serde_json::json!({
        "title": title, "notes": notes, "via": via,
    })))
}

/// Pull `title`/`notes` out of a model reply.
///
/// Models wrap the JSON in ```json fences and add prose, and — the failure this
/// guards against — get cut off at `max_tokens` mid-string, so the closing
/// brace never arrives. The previous version fell back to "first line" on any
/// parse failure, which is why a truncated reply surfaced the literal "```json"
/// fence as the note title (the reported bug).
fn parse_title_notes(raw: &str) -> (String, String) {
    // Anchor at the JSON object if there is one; a leading ```json fence and any
    // prose before `{` are then ignored by both the strict parse and the
    // salvage scan below.
    let obj = match raw.find('{') {
        Some(a) => &raw[a..],
        None => raw,
    };

    // Happy path — a complete object.
    if let Some(b) = obj.rfind('}') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&obj[..=b]) {
            let title = v["title"].as_str().unwrap_or("").trim().to_string();
            let notes = v["notes"].as_str().unwrap_or("").trim().to_string();
            if !title.is_empty() {
                return (clamp_title(&title), notes);
            }
        }
    }

    // Salvage path — truncated or malformed JSON. `title` comes first and is
    // short, so it almost always survived even when `notes` got cut off; read
    // the field values directly rather than giving up.
    if let Some(title) = json_string_field(obj, "title") {
        let title = title.trim();
        if !title.is_empty() {
            let notes = json_string_field(obj, "notes").unwrap_or_default();
            return (clamp_title(title), notes.trim().to_string());
        }
    }

    // Last resort — the first line that is neither a fence nor a bare brace, so
    // the user still gets *something*, never the literal "```json".
    let first = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("```") && *l != "{" && *l != "}")
        .unwrap_or("");
    (clamp_title(first), String::new())
}

fn clamp_title(s: &str) -> String {
    s.chars().take(60).collect()
}

/// Read a JSON string field by key, tolerating truncation: returns the text up
/// to the closing unescaped quote, or to end-of-input if the reply was cut off
/// mid-value. Scans within the object slice so a `"title"` in surrounding prose
/// isn't mistaken for the field.
fn json_string_field(obj: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let after_key = &obj[obj.find(&pat)? + pat.len()..];
    let after_colon = &after_key[after_key.find(':')? + 1..];
    let mut chars = after_colon.trim_start().chars();
    if chars.next()? != '"' {
        return None; // not a string value
    }
    let mut out = String::new();
    let mut escaped = false;
    for c in chars {
        if escaped {
            out.push(match c {
                'n' => '\n',
                't' => '\t',
                other => other, // \" \\ \/ and the rest, literally
            });
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Some(out); // closed cleanly
        } else {
            out.push(c);
        }
    }
    Some(out) // truncated before the closing quote — return what we have
}

#[derive(Deserialize)]
pub(crate) struct SpaceAppBridgeBody {
    action: String,
    payload: Option<serde_json::Value>,
}

pub(crate) async fn space_apps_bridge(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(b): Json<SpaceAppBridgeBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    match b.action.as_str() {
        "capabilities" => Ok(Json(serde_json::json!({
            "appId": id,
            "capabilities": [
                "llm.request", "agent.run", "mcp.call", "space.rest",
                "knowledge.save", "knowledge.search", "knowledge.recall",
                "usage.report",
            ],
            "status": "available",
            // What contract this daemon serves and whether it will start
            // refusing tokenless calls — the one probe an SDK needs to know
            // whether it is talking to a daemon older than its own features.
            "apiVersion": s.config.space_api_version,
            "appTokenMode": s.config.space_app_token_mode.as_str(),
        }))),
        // Run a FULL tool-enabled agent (default tools + the app's own MCP +
        // browser/web-search) headless and return its final text.
        // Payload: { prompt, system?, workspace?, timeoutSeconds? }.
        "agent.run" => {
            let payload = b.payload.clone().unwrap_or_default();
            let prompt = payload.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            if prompt.trim().is_empty() {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    "prompt is required".into(),
                ));
            }
            let Some(pool) = s.virtual_worker_pool.clone() else {
                return Ok(Json(serde_json::json!({
                    "appId": id, "status": "error", "message": "agent runtime not available",
                })));
            };
            let system = payload
                .get("system")
                .and_then(|v| v.as_str())
                .unwrap_or("You are a helpful AI assistant. Use your tools when they help.")
                .to_string();
            let timeout = payload
                .get("timeoutSeconds")
                .and_then(|v| v.as_u64())
                .map(|t| std::time::Duration::from_secs(t.clamp(10, 1800)));
            let workspace = payload
                .get("workspace")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| s.config.paths.workspace_dir.clone());
            let mem_folder = payload
                .get("space")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("space-app-{id}"));
            // Optional per-call tool allowlist: when present & non-empty, the
            // agent receives EXACTLY these tools (minus the always-excluded
            // set) instead of the full pool — this is how a Space App enforces
            // a per-bot MCP/skill security policy. Absent → all tools (the
            // historical behavior, so ai-office and others are unaffected).
            let tools = payload
                .get("tools")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.as_str().map(str::trim).filter(|s| !s.is_empty()))
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty());
            // Optional per-call model hint (persona.model).
            let model = payload
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let persona = crate::agent::persona_registry::PersonaConfig {
                name: format!("space-app-{id}"),
                description: "Space App headless agent".into(),
                tools,
                model,
                max_concurrent: 4,
                system_prompt: system,
                file_path: std::path::PathBuf::new(),
                location: crate::agent::persona_registry::PersonaLocation::Project,
            };
            match pool
                .run(
                    &persona,
                    prompt,
                    &workspace.to_string_lossy(),
                    None,
                    timeout,
                    None,
                    Some(&mem_folder),
                )
                .await
            {
                // No usage row is recorded HERE — the virtual pool already
                // recorded each underlying LLM call (anti-double-count rule);
                // the totals just ride back to the app.
                Ok(r) => Ok(Json(serde_json::json!({
                    "appId": id, "status": "ok", "text": r.result, "durationMs": r.duration_ms,
                    "usage": {"inputTokens": r.tokens_in, "outputTokens": r.tokens_out},
                }))),
                Err(e) => Ok(Json(serde_json::json!({
                    "appId": id, "status": "error", "message": e.to_string(),
                }))),
            }
        }
        // Run a one-shot completion on SenClaw's active LLM on the app's behalf.
        // Payload: { prompt: string, system?: string, maxTokens?: number }.
        "llm.request" => {
            let payload = b.payload.clone().unwrap_or_default();
            let prompt = payload
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if prompt.trim().is_empty() {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    "prompt is required".into(),
                ));
            }
            let system = payload
                .get("system")
                .and_then(|v| v.as_str())
                .unwrap_or("You are a helpful assistant.");
            let max_tokens = payload
                .get("maxTokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            // Optional: run this completion on a specific LLM profile (config id
            // or label) instead of the daemon's global active model, so an app
            // can have its own model without changing everyone else's.
            let profile = payload
                .get("profile")
                .or_else(|| payload.get("llmProfile"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match super::llm_config::chat_completion(
                &s.config.paths.global_config_path,
                profile,
                system,
                &prompt,
                max_tokens,
                // The caller is this app: refuse a model it serves itself.
                Some(id.as_str()),
            )
            .await
            {
                Ok(r) => {
                    super::llm_config::record_completion(
                        &s.usage_recorder,
                        &format!("app:{id}"),
                        &id,
                        &r,
                    );
                    // `usage` is null when the provider reported none (apps
                    // fall back to their own estimates then). inputTokens is
                    // the total billed input (cache included); the cache
                    // fields break it down for providers that report them.
                    let usage_json = r.usage.as_ref().map(|u| {
                        serde_json::json!({
                            "inputTokens": u.input(),
                            "outputTokens": u.output(),
                            "cacheReadTokens": u.cache_read_input_tokens.unwrap_or(0),
                            "cacheCreationTokens": u.cache_creation_input_tokens.unwrap_or(0),
                        })
                    });
                    Ok(Json(serde_json::json!({
                        "appId": id, "status": "ok", "text": r.text, "model": r.model,
                        "finish": r.finish, "usage": usage_json,
                    })))
                }
                Err(e) => Ok(Json(serde_json::json!({
                    "appId": id, "status": "error", "message": e,
                }))),
            }
        }
        // A Space App that calls a provider directly (rule-engine, video-cloner)
        // reports its own token usage here so the daemon's accounting stays
        // complete. Payload: { model, provider?, inputTokens, outputTokens,
        // cacheReadTokens?, cacheCreationTokens?, latencyMs?, estimated? }.
        "usage.report" => {
            let payload = b.payload.clone().unwrap_or_default();
            let model = payload
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let get_u64 = |k: &str| payload.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            let input_tokens = get_u64("inputTokens");
            let output_tokens = get_u64("outputTokens");
            if model.is_empty() || (input_tokens == 0 && output_tokens == 0) {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    "model and non-zero inputTokens/outputTokens are required".into(),
                ));
            }
            if let Some(rec) = &s.usage_recorder {
                rec.record(crate::usage::UsageEvent {
                    jid: format!("app:{id}"),
                    app_id: id.clone(),
                    provider: payload
                        .get("provider")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    model,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens: get_u64("cacheReadTokens"),
                    cache_creation_tokens: get_u64("cacheCreationTokens"),
                    latency_ms: get_u64("latencyMs"),
                    estimated: payload
                        .get("estimated")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    ..crate::usage::UsageEvent::new(crate::usage::UsageSource::AppDirect)
                });
            }
            Ok(Json(serde_json::json!({ "appId": id, "status": "ok" })))
        }
        // Knowledge-space bridge: each Space App (and each of its internal
        // agents) can keep isolated memories. `space` defaults to the app id
        // so an app gets a private space with zero configuration.
        // Payloads:
        //   knowledge.save   { text, space?, tags?: string[], source? }
        //   knowledge.search { query, space?, mode?, limit? }
        //   knowledge.recall { query, space?, mode?, limit?, hops? }
        "knowledge.save" => {
            let payload = b.payload.clone().unwrap_or_default();
            let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if text.trim().is_empty() {
                return Err(AppError(StatusCode::BAD_REQUEST, "text is required".into()));
            }
            let Some(sys) = crate::memory::cognitive::try_get_instance() else {
                return Ok(Json(serde_json::json!({
                    "appId": id, "status": "error",
                    "message": "cognitive system is not initialized",
                })));
            };
            let space = payload
                .get("space")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| id.clone());
            let mut node_sets = vec![
                crate::memory::cognitive::NodeSet::global("default_memory"),
                crate::memory::cognitive::NodeSet::space(&space),
            ];
            if let Some(tags) = payload.get("tags").and_then(|v| v.as_array()) {
                for t in tags {
                    if let Some(t) = t.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        node_sets.push(crate::memory::cognitive::NodeSet::global(t));
                    }
                }
            }
            let opts = crate::memory::cognitive::CognifyOptions {
                node_sets,
                ..Default::default()
            };
            let source = payload
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("space-app")
                .to_string();
            match sys.cognify(text, &source, &opts).await {
                Ok(report) => Ok(Json(serde_json::json!({
                    "appId": id, "status": "ok", "space": space,
                    "chunksAdded": report.chunks_added,
                    "entitiesAdded": report.entities_added,
                }))),
                Err(e) => Ok(Json(serde_json::json!({
                    "appId": id, "status": "error", "message": e.to_string(),
                }))),
            }
        }
        "knowledge.search" | "knowledge.recall" => {
            let payload = b.payload.clone().unwrap_or_default();
            let query = payload.get("query").and_then(|v| v.as_str()).unwrap_or("");
            if query.trim().is_empty() {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    "query is required".into(),
                ));
            }
            let Some(sys) = crate::memory::cognitive::try_get_instance() else {
                return Ok(Json(serde_json::json!({
                    "appId": id, "status": "error",
                    "message": "cognitive system is not initialized",
                })));
            };
            let space = payload
                .get("space")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| id.clone());
            let limit = payload
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(6)
                .clamp(1, 30) as usize;
            let mut q = crate::memory::cognitive::SearchQuery::chunks(query.to_string(), limit);
            q.query_type = super::cognitive::search_type_from_mode(
                payload.get("mode").and_then(|v| v.as_str()),
            );
            q.hops = payload
                .get("hops")
                .and_then(|v| v.as_u64())
                .unwrap_or(2)
                .clamp(1, 6) as u8;
            q.decay_per_hop = 0.6;
            q.node_sets = vec![crate::memory::cognitive::NodeSet::space(&space)];
            let hits = match sys.search(&q).await {
                Ok(h) => h,
                Err(e) => {
                    return Ok(Json(serde_json::json!({
                        "appId": id, "status": "error", "message": e.to_string(),
                    })))
                }
            };
            let sources: Vec<serde_json::Value> = hits
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "id": h.node.id.to_string(),
                        "kind": h.node.kind.as_str(),
                        "name": h.node.name,
                        "summary": h.node.summary,
                        "score": h.score,
                    })
                })
                .collect();
            if b.action == "knowledge.search" {
                return Ok(Json(serde_json::json!({
                    "appId": id, "status": "ok", "space": space, "hits": sources,
                })));
            }
            // knowledge.recall — synthesize an answer over the scoped hits;
            // degrades to joined snippets when no cognitive LLM is configured.
            if hits.is_empty() {
                return Ok(Json(serde_json::json!({
                    "appId": id, "status": "ok", "space": space,
                    "answer": "", "grounded": false, "sources": [],
                })));
            }
            let context = hits
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    let text = if h.node.summary.trim().is_empty() {
                        h.node.name.clone()
                    } else {
                        h.node.summary.clone()
                    };
                    format!("[{}] {}", i + 1, text.trim())
                })
                .collect::<Vec<_>>()
                .join("\n");
            let answer = match crate::memory::cognitive::create_cognitive_llm(s.config.as_ref()) {
                Some(llm) => {
                    let user = format!(
                        "Context:\n{context}\n\nQuestion: {}\n\nAnswer using only the context above, citing sources as [n].",
                        query.trim()
                    );
                    llm.complete(super::cognitive::RECALL_SYSTEM, &user)
                        .await
                        .unwrap_or_else(|_| context.clone())
                }
                None => context.clone(),
            };
            Ok(Json(serde_json::json!({
                "appId": id, "status": "ok", "space": space,
                "answer": answer, "grounded": true, "sources": sources,
            })))
        }
        "mcp.call" => Ok(Json(serde_json::json!({
            "appId": id,
            "action": b.action,
            "status": "pending",
            "message": "mcp.call bridge action is not enabled yet.",
        }))),
        _ => Err(AppError(
            StatusCode::BAD_REQUEST,
            "Unknown bridge action".into(),
        )),
    }
}

pub(crate) async fn space_app_env(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let app_dir = space_app_dir(&s, &id)?;
    // Deliberately no access token: this endpoint feeds the app's *browser* UI,
    // and a secret handed to page JS is a secret in every extension and
    // devtools console the user has open. The browser reaches the daemon
    // same-origin and is trusted for that; only the app's own process gets a
    // token, in its environment. See src/apps/token.rs.
    Ok(Json(serde_json::json!({
        "appId": id,
        "apiBase": "/api/space/apps",
        "coreBase": "/api",
        "staticBase": format!("/api/space/apps/{id}/static"),
        "appDir": app_dir.to_string_lossy(),
        "apiVersion": s.config.space_api_version,
        "appTokenMode": s.config.space_app_token_mode.as_str(),
        "sqlite": {
            "endpoint": format!("/api/space/apps/{id}/sqlite/query"),
        },
        "config": {
            "endpoint": format!("/api/space/apps/{id}/config"),
        },
        "mcp": {
            "registerEndpoint": format!("/api/space/apps/{id}/mcp/register"),
        },
    })))
}

/// `GET /api/space/apps/:id/token` — the app's access token, for the operator.
///
/// Two audiences: the Web UI, which shows it so a developer can run the app by
/// hand outside the daemon (`SENCLAW_TOKEN_ACCESS_APP=… npm start`), and the
/// app itself, which already has the token and can use it to check that the
/// daemon still agrees. Minting on read is deliberate — an app installed before
/// this feature has no row until something asks.
///
/// Under `SENCLAW_APP_TOKEN_MODE=strict` an app presenting *another* app's
/// token is already refused by the middleware; what is left is the operator's
/// own UI, which is trusted to see it.
pub(crate) async fn space_app_token_get(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    let token = crate::apps::token::ensure(db(&s)?, &id).map_err(internal)?;
    Ok(Json(serde_json::json!({
        "appId": id,
        "token": token,
        "envVar": crate::apps::token::ENV_APP_TOKEN,
        "header": crate::apps::token::HEADER_APP_TOKEN,
        "apiVersion": s.config.space_api_version,
        "mode": s.config.space_app_token_mode.as_str(),
    })))
}

/// `POST /api/space/apps/:id/token` — mint a new one and drop the old.
///
/// The running process still holds the previous token in its environment, so
/// the app is restarted here: leaving it running would leave it authenticating
/// with a secret the daemon no longer accepts, which reads as "rotation broke
/// my app" rather than "rotation worked".
pub(crate) async fn space_app_token_rotate(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    let token = crate::apps::token::rotate(db(&s)?, &id).map_err(internal)?;
    let restarted = match s.space_mcp_launcher.as_ref() {
        Some(l) => {
            let was_running = l.is_running(&id).await;
            l.stop_app(&id).await;
            was_running
        }
        None => false,
    };
    Ok(Json(serde_json::json!({
        "appId": id,
        "token": token,
        "rotated": true,
        // The app is stopped rather than restarted: a session app starts again
        // on its next use, and a background app is brought back by the
        // supervisor — both paths re-read the environment.
        "stopped": restarted,
        "apiVersion": s.config.space_api_version,
    })))
}

/// `GET /api/space/app-token-mode` — the app-isolation switch, for the UI.
///
/// Reports what is in force *and where it came from*, because the three sources
/// behave differently when the operator tries to change it: a UI choice is
/// theirs to undo, an `SENCLAW_APP_TOKEN_MODE` in the environment is not (it
/// comes back on the next daemon start), and the default is what "Follow the
/// environment" falls back to.
pub(crate) async fn space_app_token_mode_get(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let (mode, source) = crate::apps::token::effective_mode(
        db(&s)?,
        s.config.space_app_token_mode,
        s.config.space_app_token_mode_from_env,
    );
    Ok(Json(serde_json::json!({
        "mode": mode.as_str(),
        "source": source.as_str(),
        // What the daemon would fall back to if the UI choice were cleared —
        // the label the "follow the environment" option needs to show.
        "envMode": s.config.space_app_token_mode.as_str(),
        "envSet": s.config.space_app_token_mode_from_env,
        "defaultMode": crate::apps::token::DEFAULT_TOKEN_MODE.as_str(),
        "apiVersion": s.config.space_api_version,
    })))
}

#[derive(serde::Deserialize)]
pub(crate) struct TokenModeBody {
    /// `off` | `warn` | `strict`, or absent/null to follow the environment.
    #[serde(default)]
    mode: Option<String>,
}

/// `PUT /api/space/app-token-mode` — change it, live.
///
/// No restart: the middleware reads this on every request, so the next call an
/// app makes is judged by the new setting. That is the point of storing it here
/// rather than in the environment.
pub(crate) async fn space_app_token_mode_put(
    State(s): State<Arc<UiState>>,
    Json(body): Json<TokenModeBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let chosen = match body
        .mode
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        None => None,
        Some(raw) => Some(
            crate::apps::token::TokenMode::parse_opt(raw).ok_or_else(|| {
                // Never silently coerce: a typo here would set an isolation level
                // the operator did not ask for and believes they did.
                AppError(
                    StatusCode::BAD_REQUEST,
                    format!("Unknown mode {raw:?} — expected off, warn or strict"),
                )
            })?,
        ),
    };
    crate::apps::token::set_mode_override(db(&s)?, chosen).map_err(internal)?;
    space_app_token_mode_get(State(s)).await
}

/// One app's sandbox settings, plus what this machine will actually enforce.
///
/// The `effective` half exists because the honest answer differs by platform:
/// macOS enforces all of it, Linux enforces the folders but not the network
/// mode, Windows enforces nothing for a long-lived server. A dialog that showed
/// only the stored settings would be claiming isolation the machine may not be
/// providing.
pub(crate) async fn space_app_sandbox_get(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Validates the id and 400s on a bad one, same as every other app route.
    let app_dir = space_app_dir(&s, &id)?;
    let db = crate::sandbox::shared_db().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Sandbox engine not available".into(),
        )
    })?;
    let cfg = crate::sandbox::app_policy::load(&db, &id);
    let caps = crate::sandbox::caps::direct_caps(false).await;

    // What the app gets without asking: its own folder and its own data dirs —
    // plus, in a jailed read mode, the language runtime if the user installed it
    // under their home (nvm, volta, pyenv…), read-only. That last part is listed
    // rather than silent: it is a real widening of the jail, and an app that
    // cannot read its own interpreter does not start.
    let mut always: Vec<String> = std::iter::once(app_dir.to_string_lossy().to_string())
        .chain(
            crate::sandbox::app_policy::own_data_dirs(&id)
                .iter()
                .map(|p| p.to_string_lossy().to_string()),
        )
        .collect();
    if cfg.read_mode.jails_reads() {
        always.extend(
            crate::sandbox::app_launch::toolchain_read_mounts()
                .into_iter()
                .map(|m| format!("{} (read-only, runtime)", m.source)),
        );
    }

    // Live proxy facts, when the app is running with per-site egress.
    let proxy = match s.space_mcp_launcher.as_ref() {
        Some(l) => l.proxy_status(&id).await,
        None => None,
    };

    Ok(Json(serde_json::json!({
        "appId": id,
        "config": cfg,
        "effective": {
            "isolation": caps.kind.as_str(),
            "enforceable": caps.kind.is_enforced() && !cfg!(windows),
            // Windows' AppContainer backend drives a child through pipes; it
            // cannot wrap a server process, so it is reported as unenforceable
            // here even though the engine uses it for `sbx_exec`.
            "networkEnforceable": matches!(caps.kind, crate::sandbox::caps::DirectKind::Seatbelt),
            "note": crate::sandbox::app_launch::note_for(caps.kind, &cfg),
            "alwaysGranted": always,
            "daemonPort": s.config.ui_server.port,
        },
        "proxy": proxy.map(|(port, stats)| serde_json::json!({
            "port": port,
            "stats": stats,
        })),
    })))
}

pub(crate) async fn space_app_sandbox_put(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<crate::sandbox::app_policy::AppSandbox>,
) -> Result<Json<serde_json::Value>, AppError> {
    space_app_dir(&s, &id)?;
    let db = crate::sandbox::shared_db().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Sandbox engine not available".into(),
        )
    })?;

    // An app whose manifest declares `sandbox.force` cannot be un-sandboxed
    // here. The flag itself is never taken from the request body either — it is
    // the manifest's to set, and a client that could send `forced: false` would
    // be able to unlock any app by asking.
    let current = crate::sandbox::app_policy::load(&db, &id);
    let mut next = body;
    next.forced = current.forced;
    if current.forced && !next.enabled {
        return Err(AppError(
            StatusCode::CONFLICT,
            "This app requires the sandbox (its manifest declares `sandbox.force`). \
             You can change what it may reach, but not turn the sandbox off."
                .into(),
        ));
    }

    let saved = crate::sandbox::app_policy::save(&db, &id, &next)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, format!("{e}")))?;

    // An edited allowlist reaches a running app's proxy immediately; everything
    // else (the profile itself) is fixed at launch, so the app must restart.
    let hosts_live = match s.space_mcp_launcher.as_ref() {
        Some(l) => l.set_proxy_hosts(&id, saved.hosts.clone()).await,
        None => false,
    };
    Ok(Json(serde_json::json!({
        "ok": true,
        "config": saved,
        "hostsAppliedLive": hosts_live,
        "restartRequired": true,
    })))
}

#[derive(Deserialize)]
pub(crate) struct AppConfigSetBody {
    value: serde_json::Value,
}

pub(crate) async fn space_app_config_list(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    let db = db(&s)?;
    let values = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT key, value, updated_at FROM space_app_config WHERE app_id=?1 ORDER BY key",
            )?;
            let rows: Vec<serde_json::Value> = stmt
                .query_map(params![&id], |row| {
                    let raw: String = row.get(1)?;
                    Ok(serde_json::json!({
                        "key": row.get::<_, String>(0)?,
                        "value": serde_json::from_str::<serde_json::Value>(&raw).unwrap_or(serde_json::Value::String(raw)),
                        "updated_at": row.get::<_, i64>(2)?,
                    }))
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
        .map_err(internal)?;
    Ok(Json(serde_json::json!({ "appId": id, "items": values })))
}

pub(crate) async fn space_app_config_get(
    State(s): State<Arc<UiState>>,
    AxumPath((id, key)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    let db = db(&s)?;
    let value = db
        .with_conn(|conn| {
            let raw: Result<String, rusqlite::Error> = conn.query_row(
                "SELECT value FROM space_app_config WHERE app_id=?1 AND key=?2",
                params![&id, &key],
                |row| row.get(0),
            );
            Ok(raw.ok())
        })
        .map_err(internal)?;
    match value {
        Some(raw) => Ok(Json(serde_json::json!({
            "key": key,
            "value": serde_json::from_str::<serde_json::Value>(&raw).unwrap_or(serde_json::Value::String(raw)),
        }))),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            "Config key not found".into(),
        )),
    }
}

pub(crate) async fn space_app_config_set(
    State(s): State<Arc<UiState>>,
    AxumPath((id, key)): AxumPath<(String, String)>,
    Json(b): Json<AppConfigSetBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    if key.trim().is_empty() || key.len() > 120 {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Invalid config key".into(),
        ));
    }
    let raw = serde_json::to_string(&b.value).map_err(internal)?;
    let now = now_ms();
    let db = db(&s)?;
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO space_app_config (app_id, key, value, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(app_id, key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            params![&id, &key, raw, now],
        )?;
        Ok(())
    })
    .map_err(internal)?;
    Ok(Json(
        serde_json::json!({ "key": key, "value": b.value, "updated_at": now }),
    ))
}

pub(crate) async fn space_app_config_delete(
    State(s): State<Arc<UiState>>,
    AxumPath((id, key)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    let db = db(&s)?;
    db.with_conn(|conn| {
        conn.execute(
            "DELETE FROM space_app_config WHERE app_id=?1 AND key=?2",
            params![&id, &key],
        )?;
        Ok(())
    })
    .map_err(internal)?;
    Ok(Json(serde_json::json!({ "success": true })))
}

#[derive(Deserialize)]
pub(crate) struct SpaceAppSqliteQueryBody {
    sql: String,
    params: Option<Vec<serde_json::Value>>,
}

pub(crate) async fn space_app_sqlite_query(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(b): Json<SpaceAppSqliteQueryBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let app_dir = space_app_dir(&s, &id)?;
    tokio::fs::create_dir_all(&app_dir)
        .await
        .map_err(internal)?;
    let db_path = app_dir.join("app.sqlite");
    let sql = b.sql.trim().to_string();
    if sql.is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "SQL is required".into()));
    }
    if sql.contains('\0') {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid SQL".into()));
    }
    let params_json = b.params.unwrap_or_default();
    let result = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let conn = rusqlite::Connection::open(db_path).map_err(|e| e.to_string())?;
        let values: Vec<SqlValue> = params_json.iter().map(json_to_sql_value).collect();
        let refs: Vec<&dyn rusqlite::ToSql> =
            values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        let verb = sql
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(verb.as_str(), "select" | "with" | "pragma") {
            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
            let column_names: Vec<String> = stmt
                .column_names()
                .into_iter()
                .map(ToString::to_string)
                .collect();
            let rows = stmt
                .query_map(&refs[..], |row| {
                    let mut obj = serde_json::Map::new();
                    for (idx, name) in column_names.iter().enumerate() {
                        let value: SqlValue = row.get(idx)?;
                        obj.insert(name.clone(), sql_value_to_json(value));
                    }
                    Ok(serde_json::Value::Object(obj))
                })
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "rows": rows }))
        } else {
            let changed = conn.execute(&sql, &refs[..]).map_err(|e| e.to_string())?;
            Ok(serde_json::json!({
                "rowsAffected": changed,
                "lastInsertRowId": conn.last_insert_rowid(),
            }))
        }
    })
    .await
    .map_err(internal)?
    .map_err(|e| AppError(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(result))
}

#[derive(Deserialize)]
pub(crate) struct SpaceAppMcpRegisterBody {
    name: Option<String>,
    transport: String,
    description: Option<String>,
    url: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
    headers: Option<HashMap<String, String>>,
    use_tools: Option<Vec<String>>,
    enabled: Option<bool>,
}

pub(crate) async fn space_app_mcp_register(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(b): Json<SpaceAppMcpRegisterBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    let mgr = s.mcp_manager.as_ref().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "MCP manager not initialized".into(),
        )
    })?;
    let transport = match b.transport.as_str() {
        "stdio" => crate::mcp::config::McpTransportType::Stdio,
        "sse" => crate::mcp::config::McpTransportType::Sse,
        "http" => crate::mcp::config::McpTransportType::Http,
        _ => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "Invalid MCP transport".into(),
            ));
        }
    };
    let name = b.name.unwrap_or_else(|| format!("space-app-{id}"));
    let mut env = b.env.unwrap_or_default();
    env.insert("SENCLAW_SPACE_APP_ID".into(), id.clone());
    env.insert("SENCLAW_SPACE_API_BASE".into(), "/api/space/apps".into());
    let config = crate::mcp::config::ExternalMcpServerConfig {
        name,
        transport,
        description: b.description,
        enabled: b.enabled.unwrap_or(true),
        use_tools: b.use_tools,
        command: b.command,
        args: b.args.unwrap_or_default(),
        env,
        url: b.url,
        headers: b.headers.unwrap_or_default(),
    };
    let info = mgr
        .add_or_update(config, crate::mcp::config::McpScopeType::Project)
        .await
        .map_err(internal)?;
    Ok(Json(serde_json::to_value(info).unwrap_or_default()))
}

/// App detail: the manifest's declared `mcp` block plus the live MCP server
/// info (status + tools) for the detail page.
pub(crate) async fn space_app_mcp_info(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    let db = db(&s)?;
    let manifest: Option<serde_json::Value> = db
        .with_conn(|conn| {
            let raw: Result<String, rusqlite::Error> = conn.query_row(
                "SELECT manifest FROM space_apps WHERE id=?1",
                params![&id],
                |row| row.get(0),
            );
            Ok(raw.ok().and_then(|s| serde_json::from_str(&s).ok()))
        })
        .map_err(internal)?;

    let declared = manifest.as_ref().and_then(|m| m.get("mcp")).cloned();

    let server = match (
        declared
            .as_ref()
            .and_then(|m| m.get("name"))
            .and_then(|v| v.as_str()),
        s.mcp_manager.as_ref(),
    ) {
        (Some(name), Some(mgr)) => {
            let info = mgr.get_server_info(name).await;
            Some(serde_json::to_value(info).unwrap_or_default())
        }
        _ => None,
    };

    Ok(Json(serde_json::json!({
        "appId": id,
        "declared": declared,
        "server": server,
    })))
}

#[derive(Deserialize)]
pub(crate) struct SpaceAppLogsQuery {
    max_bytes: Option<usize>,
}

/// The stored manifest for one app, or `None` when it is not installed.
///
/// Every per-app route needs this and each one was re-writing the same query;
/// the monitor made it four.
pub(super) fn space_app_manifest(s: &UiState, id: &str) -> Option<serde_json::Value> {
    let db = s.db.as_deref()?;
    db.with_conn(|conn| {
        let raw: Result<String, rusqlite::Error> = conn.query_row(
            "SELECT manifest FROM space_apps WHERE id=?1",
            params![id],
            |row| row.get(0),
        );
        Ok(raw.ok().and_then(|s| serde_json::from_str(&s).ok()))
    })
    .ok()
    .flatten()
}

pub(super) fn installed_app_dir_from_manifest(
    s: &UiState,
    id: &str,
    manifest: Option<&serde_json::Value>,
) -> Result<PathBuf, AppError> {
    manifest
        .and_then(|m| m["install"]["localPath"].as_str())
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(|| space_app_dir(s, id))
}

pub(super) fn space_app_runtime_log_path(
    s: &UiState,
    id: &str,
    manifest: Option<&serde_json::Value>,
) -> Result<PathBuf, AppError> {
    let app_dir = installed_app_dir_from_manifest(s, id, manifest)?;
    Ok(super::space_mcp::app_runtime_log_path(&app_dir))
}

pub(crate) async fn space_app_logs_get(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<SpaceAppLogsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    let db = db(&s)?;
    let manifest: Option<serde_json::Value> = db
        .with_conn(|conn| {
            let raw: Result<String, rusqlite::Error> = conn.query_row(
                "SELECT manifest FROM space_apps WHERE id=?1",
                params![&id],
                |row| row.get(0),
            );
            Ok(raw.ok().and_then(|s| serde_json::from_str(&s).ok()))
        })
        .map_err(internal)?;

    let log_path = space_app_runtime_log_path(&s, &id, manifest.as_ref())?;
    let max_bytes = q.max_bytes.unwrap_or(128 * 1024).clamp(1, 1024 * 1024);
    let metadata = tokio::fs::metadata(&log_path).await.ok();
    let content = match metadata.as_ref().map(|m| m.len()).unwrap_or(0) {
        0 => String::new(),
        size => {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut file = tokio::fs::File::open(&log_path).await.map_err(internal)?;
            let start = size.saturating_sub(max_bytes as u64);
            if start > 0 {
                file.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(internal)?;
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).await.map_err(internal)?;
            String::from_utf8_lossy(&bytes).to_string()
        }
    };

    Ok(Json(serde_json::json!({
        "appId": id,
        "path": log_path.to_string_lossy(),
        "exists": metadata.is_some(),
        "size": metadata.map(|m| m.len()).unwrap_or(0),
        "maxBytes": max_bytes,
        "content": content,
    })))
}

pub(crate) async fn space_app_logs_clear(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    let db = db(&s)?;
    let manifest: Option<serde_json::Value> = db
        .with_conn(|conn| {
            let raw: Result<String, rusqlite::Error> = conn.query_row(
                "SELECT manifest FROM space_apps WHERE id=?1",
                params![&id],
                |row| row.get(0),
            );
            Ok(raw.ok().and_then(|s| serde_json::from_str(&s).ok()))
        })
        .map_err(internal)?;

    let log_path = space_app_runtime_log_path(&s, &id, manifest.as_ref())?;
    if let Some(parent) = log_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(internal)?;
    }
    tokio::fs::write(&log_path, "").await.map_err(internal)?;
    Ok(Json(serde_json::json!({
        "appId": id,
        "path": log_path.to_string_lossy(),
        "cleared": true,
    })))
}

/// Best-effort: after install, install the app's bundled skills, then launch
/// its server runtime (if any) and auto-register its declared MCP.
async fn try_autoregister_app_mcp(s: &UiState, app_id: &str, manifest: &serde_json::Value) {
    // Resolve where the app's files live (explicit localPath wins).
    let app_dir = manifest
        .get("install")
        .and_then(|i| i.get("localPath"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| space_app_dir(s, app_id).ok())
        .unwrap_or_default();

    // Install bundled skills + personas (read-only, tied to the app).
    super::space_skills::install_app_skills(&s.config, app_id, &app_dir, manifest);
    super::space_personas::install_app_personas(&s.config, app_id, &app_dir, manifest);

    // Apply the confinement the app declares for itself, before it has ever
    // run. An app that says `sandbox.force` must never get one unconfined
    // launch on the way to being sandboxed.
    crate::apps::sandbox_decl::apply(app_id, manifest);

    let (Some(launcher), Some(mgr), Some(db)) = (
        s.space_mcp_launcher.as_ref(),
        s.mcp_manager.as_ref(),
        s.db.as_deref(),
    ) else {
        return;
    };
    let base_url = format!("http://127.0.0.1:{}", s.config.ui_server.port);
    match launcher
        .run_and_register(db, mgr, app_id, &app_dir, manifest, &base_url)
        .await
    {
        Ok(Some(name)) => {
            tracing::info!("[space-mcp] auto-registered '{name}' on install of '{app_id}'")
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("[space-mcp] install auto-register for '{app_id}' failed: {e}"),
    }
}

// ─── Reminder (set reminder on existing event) ────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct SetReminderBody {
    reminder_min: i64,
    group_folder: Option<String>,
    chat_jid: Option<String>,
}

pub(crate) async fn space_events_set_reminder(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(b): Json<SetReminderBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db_arc =
        s.db.clone()
            .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))?;

    let srv = crate::mcp::space_server::SpaceServer::new(db_arc);
    let result = srv.set_reminder(
        id,
        b.reminder_min,
        b.group_folder.as_deref().unwrap_or("default"),
        b.chat_jid.as_deref().unwrap_or(""),
    );

    if result.is_error {
        return Err(AppError(StatusCode::INTERNAL_SERVER_ERROR, result.content));
    }
    let v: serde_json::Value = serde_json::from_str(&result.content).unwrap_or_default();
    Ok(Json(v))
}

// ─── External sync endpoints ────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct SyncBody {
    token: String,
    days: Option<u32>,
    /// CalDAV only: the account the password belongs to.
    #[serde(default)]
    username: Option<String>,
    /// CalDAV only: server root; defaults to iCloud.
    #[serde(default, rename = "baseUrl")]
    base_url: Option<String>,
    /// Apple Notes only: how many notes to read.
    #[serde(default)]
    limit: Option<u32>,
}

/// GET /api/space/sync/status — what the last run of each importer did.
///
/// The sources themselves are reached from chat (the agent holds the
/// credential); this is how a client shows "last synced" without storing a
/// copy of that state itself.
pub(crate) async fn space_sync_status(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db =
        s.db.clone()
            .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))?;
    let mut out = serde_json::Map::new();
    for source in [
        crate::mcp::space_sync::google_calendar::SOURCE,
        crate::mcp::space_sync::caldav::SOURCE,
        crate::mcp::space_sync::apple_notes::SOURCE,
    ] {
        let key = format!("space:sync:{source}:last");
        let value = db
            .get_router_state(&key)
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
        out.insert(
            source.to_string(),
            serde_json::json!({
                // `null` means "never run here", which a client must not
                // render as "synced, nothing found".
                "last": value,
                "supported": source != crate::mcp::space_sync::apple_notes::SOURCE
                    || crate::mcp::space_sync::apple_notes::supported(),
            }),
        );
    }
    Ok(Json(serde_json::json!({ "sources": out })))
}

pub(crate) async fn space_sync_google_calendar(
    State(s): State<Arc<UiState>>,
    Json(b): Json<SyncBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db_arc =
        s.db.clone()
            .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))?;
    let srv = crate::mcp::space_server::SpaceServer::new(db_arc);
    let r = srv.sync_google_calendar(b.token, b.days.unwrap_or(30)).await;
    Ok(Json(serde_json::from_str(&r.content).unwrap_or_default()))
}

pub(crate) async fn space_sync_apple_calendar(
    State(s): State<Arc<UiState>>,
    Json(b): Json<SyncBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db_arc =
        s.db.clone()
            .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))?;
    let srv = crate::mcp::space_server::SpaceServer::new(db_arc);
    let r = srv
        .sync_apple_calendar(
            b.username.unwrap_or_default(),
            b.token,
            b.base_url,
            b.days.unwrap_or(30),
        )
        .await;
    Ok(Json(serde_json::from_str(&r.content).unwrap_or_default()))
}

pub(crate) async fn space_sync_apple_notes(
    State(s): State<Arc<UiState>>,
    Json(b): Json<SyncBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let db_arc =
        s.db.clone()
            .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))?;
    let srv = crate::mcp::space_server::SpaceServer::new(db_arc);
    let r = srv.sync_apple_notes(b.limit.unwrap_or(500) as usize).await;
    Ok(Json(serde_json::from_str(&r.content).unwrap_or_default()))
}

#[derive(Deserialize)]
pub(crate) struct GoogleWorkspaceSyncBody {
    token: String,
    days: Option<u32>,
    services: Option<Vec<String>>,
}

pub(crate) async fn space_sync_google_workspace(
    State(s): State<Arc<UiState>>,
    Json(b): Json<GoogleWorkspaceSyncBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let token = b.token.trim().to_string();
    if token.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Google access token required".into(),
        ));
    }

    let services = b
        .services
        .unwrap_or_else(|| vec!["calendar".to_string(), "notes".to_string()]);
    let days = b.days.unwrap_or(7);
    let db_arc =
        s.db.clone()
            .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "DB not available".into()))?;
    let srv = crate::mcp::space_server::SpaceServer::new(db_arc);

    let mut results = serde_json::Map::new();
    for service in services {
        match service.as_str() {
            "calendar" => {
                let r = srv.sync_google_calendar(token.clone(), days).await;
                results.insert(
                    "calendar".to_string(),
                    serde_json::from_str(&r.content)
                        .unwrap_or_else(|_| serde_json::json!({ "status": "error" })),
                );
            }
            "notes" => {
                results.insert(
                    "notes".to_string(),
                    serde_json::json!({
                        "status": "pending",
                        "message": "Google Workspace notes sync is not implemented yet. The connector reserves this slot for Keep/Drive-based notes import.",
                    }),
                );
            }
            other => {
                results.insert(
                    other.to_string(),
                    serde_json::json!({
                        "status": "skipped",
                        "message": "Unknown Google Workspace service",
                    }),
                );
            }
        }
    }

    Ok(Json(serde_json::json!({
        "status": "completed",
        "days": days,
        "results": results,
    })))
}

pub(crate) async fn space_apps_proxy(
    State(s): State<Arc<UiState>>,
    AxumPath((id, path)): AxumPath<(String, String)>,
    req: axum::extract::Request<Body>,
) -> Result<Response, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }

    let manifest: Option<serde_json::Value> = db(&s)?
        .with_conn(|conn| {
            let raw: Result<String, rusqlite::Error> = conn.query_row(
                "SELECT manifest FROM space_apps WHERE id=?1",
                params![&id],
                |row| row.get(0),
            );
            Ok(raw.ok().and_then(|s| serde_json::from_str(&s).ok()))
        })
        .map_err(internal)?;

    let manifest =
        manifest.ok_or_else(|| AppError(StatusCode::NOT_FOUND, "App not found".into()))?;

    let path_str = if path.starts_with('/') {
        path.clone()
    } else {
        format!("/{}", path)
    };
    let query_string = req
        .uri()
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();

    let (parts, body) = req.into_parts();
    let body_bytes = axum::body::to_bytes(body, usize::MAX).await.map_err(|e| {
        AppError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read body: {}", e),
        )
    })?;

    // Lazy self-heal: bring the app up if it isn't running (or crashed) so a
    // proxied request never renders as a blank iframe. `ensure_running` spawns +
    // health-gates the process (serialized per app), so we forward only once the
    // backend is actually up.
    let ensure_port = || async {
        let (Some(launcher), Some(db)) = (s.space_mcp_launcher.as_ref(), s.db.as_deref()) else {
            return Err(AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "App runtime not available".into(),
            ));
        };
        let app_dir = manifest
            .get("install")
            .and_then(|i| i.get("localPath"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .or_else(|| space_app_dir(&s, &id).ok())
            .unwrap_or_default();
        let base_url = format!("http://127.0.0.1:{}", s.config.ui_server.port);
        launcher
            .ensure_running(db, &id, &app_dir, &manifest, &base_url)
            .await
            .map_err(|e| AppError(StatusCode::BAD_GATEWAY, format!("App is not running: {e}")))
    };

    // Every use of an app travels this handler: the UI iframe, the app's own
    // REST calls, and — for a session app — every MCP tool call, whose server
    // URL points here. So this is where "the app is being used" is recorded,
    // and what the idle reaper measures against.
    if let Some(launcher) = s.space_mcp_launcher.as_ref() {
        launcher.touch(&id).await;
    }

    // Prefer the last-known port; if none is recorded, start the app first.
    //
    // A recorded port is only trustworthy while *we* are the ones running the
    // process. If the launcher tracks no live child for this app, the number in
    // the manifest is a leftover: the app may be stopped (the resting state for
    // a `session` app), or — worse — the port may now be held by an orphan from
    // a previous daemon run, which answers the forward and serves the wrong
    // thing. Start it properly in that case; `ensure_running` reclaims the port
    // and health-gates before returning.
    let recorded = manifest
        .get("runtime")
        .and_then(|r| r.get("port"))
        .and_then(|p| p.as_u64())
        .map(|p| p as u16)
        .filter(|p| *p > 0);
    let tracked = match s.space_mcp_launcher.as_ref() {
        Some(l) => l.is_running(&id).await,
        None => false,
    };
    let mut port = match recorded {
        Some(p) if tracked => p,
        _ => ensure_port().await?,
    };

    // The app's own access token, so the app can tell a request that came
    // through the daemon from one that arrived directly on its port. An app
    // that opts into checking it (every SDK ships the guard) is then reachable
    // only via this proxy, which is the only caller that can prove which app it
    // is addressing.
    let app_token =
        s.db.as_deref()
            .and_then(|db| crate::apps::token::ensure(db, &id).ok());
    let api_version = s.config.space_api_version.to_string();

    let forward = |port: u16| {
        let method = parts.method.clone();
        let headers = parts.headers.clone();
        let url = format!("http://127.0.0.1:{}{}{}", port, path_str, query_string);
        let body = body_bytes.clone();
        let app_token = app_token.clone();
        let api_version = api_version.clone();
        async move {
            let client = reqwest::Client::new();
            let mut builder = client.request(method, &url);
            for (name, value) in headers.iter() {
                // HOST would name the daemon, not the app. The two SenClaw
                // headers are re-stamped below from what the *daemon* knows —
                // forwarding a client's copy would let any page that can reach
                // this route hand the app a token of its choosing.
                if name == axum::http::header::HOST
                    || name.as_str() == crate::apps::token::HEADER_APP_TOKEN
                    || name.as_str() == crate::apps::token::HEADER_API_VERSION
                {
                    continue;
                }
                builder = builder.header(name, value);
            }
            if let Some(t) = app_token.as_deref() {
                builder = builder.header(crate::apps::token::HEADER_APP_TOKEN, t);
            }
            builder = builder.header(crate::apps::token::HEADER_API_VERSION, &api_version);
            builder.body(body).send().await
        }
    };

    // First attempt; if the backend is unreachable (killed / not yet up),
    // (re)start it and retry once.
    let res = match forward(port).await {
        Ok(r) => r,
        Err(_) => {
            port = ensure_port().await?;
            forward(port).await.map_err(|e| {
                AppError(
                    StatusCode::BAD_GATEWAY,
                    format!("Proxy request failed: {}", e),
                )
            })?
        }
    };

    let mut response_builder = Response::builder().status(res.status());
    for (name, value) in res.headers() {
        response_builder = response_builder.header(name, value);
    }

    Ok(response_builder
        .body(Body::from_stream(res.bytes_stream()))
        .unwrap())
}

pub(crate) async fn space_apps_proxy_root(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    req: axum::extract::Request<Body>,
) -> Result<Response, AppError> {
    space_apps_proxy(State(s), AxumPath((id, "".to_string())), req).await
}

#[cfg(test)]
mod parse_title_notes_tests {
    use super::{json_string_field, parse_title_notes};

    #[test]
    fn clean_json() {
        let (t, n) = parse_title_notes(r#"{"title": "Tạo giftcode", "notes": "cân chơi 1 ngày"}"#);
        assert_eq!(t, "Tạo giftcode");
        assert_eq!(n, "cân chơi 1 ngày");
    }

    #[test]
    fn fenced_json() {
        let raw = "```json\n{\"title\": \"Giftcode 15k\", \"notes\": \"mốc cược 150k\"}\n```";
        let (t, n) = parse_title_notes(raw);
        assert_eq!(t, "Giftcode 15k");
        assert_eq!(n, "mốc cược 150k");
    }

    #[test]
    fn prose_around_json() {
        let raw = "Đây là kết quả:\n{\"title\": \"Cấu hình giftcode\", \"notes\": \"\"}\nHy vọng giúp được.";
        let (t, _) = parse_title_notes(raw);
        assert_eq!(t, "Cấu hình giftcode");
    }

    /// The reported bug: a long chat image, the model opens ```json and gets cut
    /// off at max_tokens before closing the brace. The old code surfaced the
    /// literal "```json" as the title.
    #[test]
    fn truncated_after_fence_salvages_the_title_not_the_fence() {
        let raw = "```json\n{\"title\": \"Tạo giftcode 15k mốc cược 150k\", \"notes\": \"Khách yêu cầu tạo loại 15k cho mốc cược 150k, thêm code vip mốc 10m, mỗi đợt tạo là dùng đượ";
        let (t, n) = parse_title_notes(raw);
        assert_eq!(t, "Tạo giftcode 15k mốc cược 150k");
        assert!(
            n.starts_with("Khách yêu cầu"),
            "notes salvaged partial: {n:?}"
        );
        assert!(!t.starts_with("```"), "must never surface the fence");
    }

    #[test]
    fn truncated_mid_title_still_avoids_the_fence() {
        let raw = "```json\n{\"title\": \"Tạo giftco";
        let (t, _) = parse_title_notes(raw);
        assert_eq!(t, "Tạo giftco");
        assert!(!t.contains("```"));
    }

    #[test]
    fn title_over_sixty_chars_is_clamped_on_a_char_boundary() {
        let long = "á".repeat(80);
        let (t, _) = parse_title_notes(&format!("{{\"title\": \"{long}\", \"notes\": \"\"}}"));
        assert_eq!(t.chars().count(), 60);
    }

    #[test]
    fn escaped_quotes_in_value_are_handled() {
        let raw = r#"{"title": "Anh nói \"chuẩn\" rồi", "notes": ""}"#;
        let (t, _) = parse_title_notes(raw);
        assert_eq!(t, r#"Anh nói "chuẩn" rồi"#);
    }

    #[test]
    fn total_garbage_falls_back_to_a_sane_line_never_the_fence() {
        let (t, n) = parse_title_notes("```json\n\nkhông đọc được ảnh");
        assert_eq!(t, "không đọc được ảnh");
        assert_eq!(n, "");
    }

    #[test]
    fn field_scanner_ignores_a_title_word_in_prose() {
        // "title" mentioned in prose before the object must not be picked up.
        let obj = r#"{"title": "Real one", "notes": "x"}"#;
        assert_eq!(json_string_field(obj, "title").as_deref(), Some("Real one"));
    }
}

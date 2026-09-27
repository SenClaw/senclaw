//! `/api/watches` — the chat's in-flight watches, and the button that stops one.
//!
//! A watch is armed by the agent mid-turn and then runs invisibly: no message,
//! no entry in the schedules screen (it is scoped to the chat's own folder, not
//! a `schedule_` one). That invisibility was the complaint — the user could see
//! that *something* was being waited on but had no way to inspect or cancel it.
//!
//! Scoped by `chat_jid` because a watch belongs to the conversation that armed
//! it; that is the only place a Stop button means anything.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::core::{AppError, UiState};
use crate::scheduler::watch::WatchConfig;

#[derive(serde::Deserialize)]
pub(crate) struct WatchQuery {
    /// Clients send camelCase; the snake_case alias keeps `curl` and older
    /// callers working. Without the rename the field name *is* the wire name,
    /// so every client's `chatJid` was rejected with a 400 and the strip never
    /// loaded — found by calling the endpoint, not by any type check.
    #[serde(rename = "chatJid", alias = "chat_jid")]
    chat_jid: String,
}

fn db(s: &Arc<UiState>) -> Result<Arc<crate::db::Db>, AppError> {
    s.db.clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "db_unset".into()))
}

/// GET /api/watches?chatJid=… — what this chat is currently waiting on.
///
/// Progress is reported as checks-so-far against the ceiling plus the give-up
/// time, because "still waiting" alone cannot be told apart from "stuck".
pub(crate) async fn watches_list(
    State(s): State<Arc<UiState>>,
    Query(q): Query<WatchQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tasks = db(&s)?
        .get_active_watches(&q.chat_jid)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let items: Vec<serde_json::Value> = tasks
        .iter()
        .map(|t| {
            // A row whose config will not parse is still shown — it is running,
            // and hiding it would leave the user with an uncancellable watch.
            let cfg = t
                .watch_json
                .as_deref()
                .and_then(|j| WatchConfig::parse(j).ok());
            serde_json::json!({
                "id": t.id,
                "label": cfg.as_ref().and_then(|c| c.label.clone()),
                "tool": cfg.as_ref().and_then(|c| c.tool.clone()),
                "checks": cfg.as_ref().map(|c| c.checks).unwrap_or(0),
                "maxChecks": cfg.as_ref().map(|c| c.max_checks).unwrap_or(0),
                "givesUpAt": cfg.as_ref().map(|c| c.deadline_at.clone()),
                "lastError": cfg.as_ref().and_then(|c| c.last_error.clone()),
                "intervalSecs": t.schedule_value.parse::<i64>().unwrap_or(0) / 1000,
                "nextCheck": t.next_run,
                "createdAt": t.created_at,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "watches": items })))
}

/// POST /api/watches/:id/stop — cancel one watch.
///
/// Completed rather than deleted: the row is the only record that the wait
/// happened, and a person who stops a watch usually wants to know later that
/// they did. Stopping is silent by design — the user just cancelled it, so
/// waking the chat to announce that would be noise.
pub(crate) async fn watch_stop(
    State(s): State<Arc<UiState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let stopped = db(&s)?
        .stop_watch(&id)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !stopped {
        // Reporting success for an id that matched nothing would tell the user
        // a watch was cancelled while it kept running.
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("no running watch {id}"),
        ));
    }
    Ok(Json(serde_json::json!({ "success": true, "id": id })))
}

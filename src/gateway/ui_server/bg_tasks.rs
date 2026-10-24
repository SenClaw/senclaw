//! `/api/bg-tasks` — what an agent left running, and the buttons that end it.
//!
//! A server started from a chat (`npm run dev`, `python -m http.server`) used to
//! be invisible: the turn ended, the process lived on, and nothing listed it or
//! stopped it. [`crate::bg_tasks`] keeps the registry; this is the window onto it
//! for the web and desktop clients — a strip above the composer (this chat's
//! tasks, `?chatJid=`) and an all-chats list (no filter).
//!
//! Stopping goes through the same path as the agent's `StopBgJob`, so a task is
//! only ever ended by signalling a process group the registry can prove is its own.

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;

use super::core::AppError;
use crate::bg_tasks;
use crate::tools::bg_jobs::{BgJobManager, JobStatus};

/// Output returned with one task, unless the caller asks for another length.
const DEFAULT_TAIL: usize = 8_000;
const MAX_TAIL: usize = 200_000;

pub(crate) fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/bg-tasks", get(list))
        // Literal siblings of `:id`; ids look like `bash-00012`, never these words.
        .route("/api/bg-tasks/stop-all", post(stop_all))
        .route("/api/bg-tasks/clear-finished", post(clear_finished))
        .route("/api/bg-tasks/:id", get(detail).delete(dismiss))
        .route("/api/bg-tasks/:id/stop", post(stop_one))
}

#[derive(serde::Deserialize)]
pub(crate) struct ListQuery {
    /// Clients send camelCase; the snake_case alias keeps `curl` working. Without
    /// the rename the field name *is* the wire name (`/api/watches` shipped that
    /// bug: every `?chatJid=` was a 400 and the strip never loaded).
    #[serde(rename = "chatJid", alias = "chat_jid", default)]
    chat_jid: Option<String>,
    #[serde(rename = "includeFinished", alias = "include_finished", default)]
    include_finished: bool,
}

#[derive(serde::Deserialize)]
pub(crate) struct TailQuery {
    #[serde(default)]
    tail: Option<usize>,
}

/// GET /api/bg-tasks?chatJid=…&includeFinished=true — newest first.
pub(crate) async fn list(Query(q): Query<ListQuery>) -> Json<serde_json::Value> {
    let chat = q.chat_jid.as_deref().filter(|c| !c.is_empty());
    Json(serde_json::json!({ "tasks": bg_tasks::views(chat, q.include_finished) }))
}

fn job(id: &str) -> Result<std::sync::Arc<crate::tools::bg_jobs::BgJob>, AppError> {
    BgJobManager::global()
        .get(id)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("no background task {id}")))
}

/// GET /api/bg-tasks/:id?tail=N — one task with the end of its output.
pub(crate) async fn detail(
    Path(id): Path<String>,
    Query(q): Query<TailQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let j = job(&id)?;
    let tail = q.tail.unwrap_or(DEFAULT_TAIL).min(MAX_TAIL);
    Ok(Json(serde_json::json!({ "task": bg_tasks::view(&j, Some(tail)) })))
}

/// POST /api/bg-tasks/:id/stop — end the task and everything it started.
pub(crate) async fn stop_one(Path(id): Path<String>) -> Result<Json<serde_json::Value>, AppError> {
    let j = job(&id)?;
    if j.status() != JobStatus::Running {
        // Reporting success for something that was not running would tell the
        // person they stopped it.
        return Err(AppError(
            StatusCode::CONFLICT,
            format!("{id} is not running ({})", j.status().as_str()),
        ));
    }
    let gone = j.kill().await;
    if !gone && j.status() == JobStatus::Running {
        return Err(AppError(
            StatusCode::CONFLICT,
            format!("{id} did not exit; try again"),
        ));
    }
    Ok(Json(serde_json::json!({
        "success": true,
        "id": id,
        "task": bg_tasks::view(&j, None),
    })))
}

/// POST /api/bg-tasks/stop-all?chatJid=… — every running task, of one chat or all.
pub(crate) async fn stop_all(Query(q): Query<ListQuery>) -> Json<serde_json::Value> {
    let chat = q.chat_jid.filter(|c| !c.is_empty());
    let mut stopped = 0usize;
    for t in BgJobManager::global().list() {
        let mine = chat.as_deref().map_or(true, |c| t.meta.chat_jid == c);
        if mine && t.status() == JobStatus::Running && t.kill().await {
            stopped += 1;
        }
    }
    Json(serde_json::json!({ "success": true, "stopped": stopped }))
}

/// DELETE /api/bg-tasks/:id — forget a finished task.
pub(crate) async fn dismiss(Path(id): Path<String>) -> Result<Json<serde_json::Value>, AppError> {
    let j = job(&id)?;
    if j.status() == JobStatus::Running {
        return Err(AppError(
            StatusCode::CONFLICT,
            format!("{id} is still running; stop it first"),
        ));
    }
    BgJobManager::global().remove(&id);
    Ok(Json(serde_json::json!({ "success": true, "id": id })))
}

/// POST /api/bg-tasks/clear-finished?chatJid=… — forget every finished task.
pub(crate) async fn clear_finished(Query(q): Query<ListQuery>) -> Json<serde_json::Value> {
    let chat = q.chat_jid.filter(|c| !c.is_empty());
    let mgr = BgJobManager::global();
    let mut removed = 0usize;
    for t in mgr.list() {
        let mine = chat.as_deref().map_or(true, |c| t.meta.chat_jid == c);
        if mine && t.status() != JobStatus::Running && mgr.remove(&t.id) {
            removed += 1;
        }
    }
    Json(serde_json::json!({ "success": true, "removed": removed }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn call(method: &str, uri: &str) -> (StatusCode, serde_json::Value) {
        let app: Router = routes();
        let res = app
            .oneshot(Request::builder().method(method).uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    fn ids(v: &serde_json::Value) -> Vec<String> {
        v["tasks"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap().to_string()).collect()
    }

    #[tokio::test]
    async fn list_detail_stop_dismiss_walk_one_task_through_its_life() {
        let job = bg_tasks::start_background(bg_tasks::StartSpec {
            command: "echo up; sleep 60".into(),
            cwd: "/tmp".into(),
            description: "rest walk".into(),
            chat_jid: "web:rest-1".into(),
        })
        .await
        .unwrap();

        // The camelCase wire name works, and another chat sees nothing.
        let (st, body) = call("GET", "/api/bg-tasks?chatJid=web:rest-1").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(ids(&body), vec![job.id.clone()]);
        assert_eq!(body["tasks"][0]["description"], "rest walk");
        assert_eq!(body["tasks"][0]["status"], "running");
        let (_, other) = call("GET", "/api/bg-tasks?chatJid=web:rest-other").await;
        assert!(ids(&other).is_empty());

        // The detail carries the output.
        for _ in 0..40 {
            if job.output_snapshot().contains("up") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let (st, d) = call("GET", &format!("/api/bg-tasks/{}", job.id)).await;
        assert_eq!(st, StatusCode::OK);
        assert!(d["task"]["output"].as_str().unwrap().contains("up"));

        // Dismissing a running task is refused; stopping it works once.
        let (st, _) = call("DELETE", &format!("/api/bg-tasks/{}", job.id)).await;
        assert_eq!(st, StatusCode::CONFLICT);
        let (st, s) = call("POST", &format!("/api/bg-tasks/{}/stop", job.id)).await;
        assert_eq!(st, StatusCode::OK, "{s}");
        assert_eq!(s["task"]["status"], "stopped");
        let (st, _) = call("POST", &format!("/api/bg-tasks/{}/stop", job.id)).await;
        assert_eq!(st, StatusCode::CONFLICT, "a second stop must not claim success");

        // Finished tasks are hidden unless asked for, and can be dismissed.
        let (_, hidden) = call("GET", "/api/bg-tasks?chatJid=web:rest-1").await;
        assert!(ids(&hidden).is_empty());
        let (_, shown) = call("GET", "/api/bg-tasks?chatJid=web:rest-1&includeFinished=true").await;
        assert_eq!(ids(&shown), vec![job.id.clone()]);
        let (st, _) = call("DELETE", &format!("/api/bg-tasks/{}", job.id)).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call("GET", &format!("/api/bg-tasks/{}", job.id)).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn stop_all_is_scoped_to_the_chat_it_names() {
        let mine = bg_tasks::start_background(bg_tasks::StartSpec {
            command: "sleep 60".into(),
            cwd: "/tmp".into(),
            description: String::new(),
            chat_jid: "web:rest-2".into(),
        })
        .await
        .unwrap();
        let theirs = bg_tasks::start_background(bg_tasks::StartSpec {
            command: "sleep 60".into(),
            cwd: "/tmp".into(),
            description: String::new(),
            chat_jid: "web:rest-3".into(),
        })
        .await
        .unwrap();
        let (st, body) = call("POST", "/api/bg-tasks/stop-all?chatJid=web:rest-2").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["stopped"], 1);
        assert_eq!(mine.status(), JobStatus::Stopped);
        assert_eq!(theirs.status(), JobStatus::Running, "another chat's task must survive");
        theirs.kill().await;
    }

    #[tokio::test]
    async fn an_unknown_id_is_a_404() {
        let (st, _) = call("POST", "/api/bg-tasks/bash-99999/stop").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
}

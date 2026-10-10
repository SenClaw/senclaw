//! App workers — a Space App that *executes* dispatched work.
//!
//! The daemon's `MCPDispatcher` claims items from its sources (the kanban
//! board, …). When an item's assignee is one an installed app declares in its
//! manifest, the dispatcher hands the item to that app instead of running a
//! SenClaw persona:
//!
//! ```json
//! "workers": [{ "assignee": "claude-code", "aliases": ["claude"], "path": "/api/worker" }]
//! ```
//!
//! The exchange is asynchronous, so a run can outlive any single HTTP request
//! (and a restart of either side):
//!
//! - `POST {path}/runs` with a [`Job`] → `{ "run_id": … }`
//! - `GET {path}/runs/{run_id}` → [`RunStatus`] (polled until `outcome` is set)
//! - `DELETE {path}/runs/{run_id}` → cancel
//!
//! The daemon creates the workspace (a git worktree when the item asks for
//! one), keeps the item's lease alive while it polls, and finalizes the item
//! with the [`Outcome`] the app reports. Every call carries the app's own
//! access token, so [`worker_router`] answers only the daemon.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::dispatch::Outcome;

/// An MCP server the worker should give its agent, reached over HTTP (the
/// daemon's `/mcp/*` bridge). `headers` carry a token scoped to this job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobMcp {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// The worktree the daemon made for this job, when it made one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobWorktree {
    pub path: String,
    pub branch: String,
    /// What the branch was cut from — diff against it to see the job's work.
    pub base: String,
}

/// One unit of work handed to an app worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Source-scoped item id (a kanban card id for the built-in board).
    pub item_id: String,
    /// The assignee that routed the item here.
    pub assignee: String,
    /// The task (title + description for a card).
    pub prompt: String,
    /// Directory to work in — the worktree path when there is one.
    pub working_dir: String,
    #[serde(default)]
    pub worktree: Option<JobWorktree>,
    /// SenClaw tools for the worker's agent.
    #[serde(default)]
    pub mcp: Vec<JobMcp>,
    /// Whole-job deadline, in seconds.
    pub timeout_secs: u64,
}

/// Body of `POST /runs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Started {
    pub run_id: String,
}

/// Body of `GET /runs/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStatus {
    pub run_id: String,
    /// Free-form phase for display (`running`, `verifying`, …).
    #[serde(default)]
    pub phase: String,
    /// Latest progress note for display.
    #[serde(default)]
    pub activity: String,
    /// Set once the run is over; the daemon finalizes the item with it.
    #[serde(default)]
    pub outcome: Option<Outcome>,
}

/// What an app implements to execute jobs.
#[async_trait]
pub trait Worker: Send + Sync {
    /// Start `job` in the background and return its run id at once.
    async fn start(&self, job: Job) -> anyhow::Result<String>;
    /// The run's status, or `None` for an unknown id (the daemon treats that
    /// as a lost run — e.g. the app restarted — and fails the item).
    async fn status(&self, run_id: &str) -> Option<RunStatus>;
    /// Stop a run. Unknown ids are not an error.
    async fn cancel(&self, run_id: &str);
}

/// `/runs`, `/runs/{id}` — nest it at the manifest's worker `path`:
///
/// ```ignore
/// let app = Router::new().nest("/api/worker", worker_router(Arc::new(MyWorker)));
/// ```
///
/// Guarded by the app's access token: only the daemon can start or read runs.
/// Unlike the general guard, a missing token closes the routes instead of
/// opening them — starting a run executes an agent, which must never be
/// anonymous (e.g. an app started by hand outside SenClaw).
pub fn worker_router(worker: Arc<dyn Worker>) -> Router {
    worker_router_with_token(worker, crate::bridge::app_token_from_env())
}

fn worker_router_with_token(worker: Arc<dyn Worker>, token: Option<String>) -> Router {
    let token = Arc::new(token);
    Router::new()
        .route("/runs", post(start))
        .route("/runs/:id", get(status).delete(cancel))
        .with_state(worker)
        .layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
            let token = Arc::clone(&token);
            async move {
                match token.as_deref() {
                    Some(t) if crate::auth::authorized(&req, Some(t), &[]) => next.run(req).await,
                    Some(_) => err(StatusCode::UNAUTHORIZED, "only the SenClaw daemon may drive this worker"),
                    None => err(StatusCode::FORBIDDEN, "worker disabled: no app access token (not started by SenClaw)"),
                }
            }
        }))
}

fn err(code: StatusCode, msg: impl ToString) -> Response {
    (code, Json(serde_json::json!({ "error": msg.to_string() }))).into_response()
}

async fn start(State(w): State<Arc<dyn Worker>>, Json(job): Json<Job>) -> Response {
    match w.start(job).await {
        Ok(run_id) => Json(Started { run_id }).into_response(),
        Err(e) => err(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")),
    }
}

async fn status(State(w): State<Arc<dyn Worker>>, Path(id): Path<String>) -> Response {
    match w.status(&id).await {
        Some(s) => Json(s).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such run"),
    }
}

async fn cancel(State(w): State<Arc<dyn Worker>>, Path(id): Path<String>) -> Response {
    w.cancel(&id).await;
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use std::sync::Mutex;
    use tower::ServiceExt;

    #[derive(Default)]
    struct Echo {
        runs: Mutex<HashMap<String, Job>>,
    }

    #[async_trait]
    impl Worker for Echo {
        async fn start(&self, job: Job) -> anyhow::Result<String> {
            let id = format!("run-{}", job.item_id);
            self.runs.lock().unwrap().insert(id.clone(), job);
            Ok(id)
        }
        async fn status(&self, run_id: &str) -> Option<RunStatus> {
            self.runs.lock().unwrap().get(run_id).map(|j| RunStatus {
                run_id: run_id.into(),
                phase: "done".into(),
                activity: String::new(),
                outcome: Some(Outcome::Completed { summary: j.prompt.clone(), metadata: serde_json::json!({}) }),
            })
        }
        async fn cancel(&self, run_id: &str) {
            self.runs.lock().unwrap().remove(run_id);
        }
    }

    fn job() -> Job {
        Job {
            item_id: "7".into(),
            assignee: "claude-code".into(),
            prompt: "fix it".into(),
            working_dir: "/tmp".into(),
            worktree: None,
            mcp: vec![],
            timeout_secs: 60,
        }
    }

    const TOKEN: &str = "sca_test";

    fn req(method: &str, uri: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header(crate::bridge::HEADER_APP_TOKEN, TOKEN)
            .body(body)
            .unwrap()
    }

    #[tokio::test]
    async fn without_a_token_or_with_a_wrong_one_nothing_runs() {
        let closed = worker_router_with_token(Arc::new(Echo::default()), None);
        let res = closed.oneshot(req("POST", "/runs", Body::from(serde_json::to_vec(&job()).unwrap()))).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let app = worker_router_with_token(Arc::new(Echo::default()), Some("sca_other".into()));
        let res = app.oneshot(req("POST", "/runs", Body::from(serde_json::to_vec(&job()).unwrap()))).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_run_starts_reports_and_cancels() {
        let app = worker_router_with_token(Arc::new(Echo::default()), Some(TOKEN.into()));
        let res = app
            .clone()
            .oneshot(req("POST", "/runs", Body::from(serde_json::to_vec(&job()).unwrap())))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let started: Started = serde_json::from_slice(&to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(started.run_id, "run-7");

        let res = app.clone().oneshot(req("GET", "/runs/run-7", Body::empty())).await.unwrap();
        let st: RunStatus = serde_json::from_slice(&to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert!(matches!(st.outcome, Some(Outcome::Completed { ref summary, .. }) if summary == "fix it"));

        let res = app.clone().oneshot(req("DELETE", "/runs/run-7", Body::empty())).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = app.oneshot(req("GET", "/runs/run-7", Body::empty())).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}

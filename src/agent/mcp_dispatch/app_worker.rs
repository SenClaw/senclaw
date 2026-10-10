//! Client side of the app-worker protocol (`app_space_sdk::worker`): hands a
//! dispatched item to the installed app that declares its assignee, then polls
//! the run until the app reports an outcome.

use std::sync::Arc;
use std::time::{Duration, Instant};

use app_space_sdk::dispatch::{McpServerSpec, Outcome, WorkItem};
use app_space_sdk::worker::{Job, JobMcp, JobWorktree, RunStatus, Started};

use crate::apps::workers::WorkerTarget;

/// How often a run is polled.
const POLL_EVERY: Duration = Duration::from_secs(3);
/// Consecutive unreachable polls tolerated (the app may be restarting).
const MAX_UNREACHABLE: u32 = 20;
/// Extra time the dispatch token outlives the job deadline.
const TOKEN_GRACE: Duration = Duration::from_secs(300);
/// The daemon waits this much past the job deadline before cancelling, so the
/// app's own (more informative) timeout outcome arrives first.
const DEADLINE_GRACE: Duration = Duration::from_secs(30);
/// Consecutive unreadable status answers tolerated before the run is failed.
const MAX_PROTOCOL_ERRORS: u32 = 5;

/// Revokes the job's dispatch token however `run` ends (panics included).
struct TokenGuard(String);

impl Drop for TokenGuard {
    fn drop(&mut self) {
        crate::mcp::http_bridge::revoke_dispatch_token(&self.0);
    }
}

/// What the dispatcher needs to drive app workers.
pub struct AppWorkerContext {
    pub db: Arc<crate::db::Db>,
    /// Daemon base URL as a local process reaches it (`http://127.0.0.1:18788`).
    pub daemon_base: String,
    pub http: reqwest::Client,
}

impl AppWorkerContext {
    pub fn new(db: Arc<crate::db::Db>, daemon_base: String) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default();
        Self { db, daemon_base, http }
    }
}

/// The SenClaw MCP servers a job's agent gets: the daemon's HTTP bridge,
/// authenticated by `token`, plus any HTTP servers the source declared
/// (native stdio specs belong to in-process workers and are not forwarded).
pub fn job_mcp(daemon_base: &str, token: &str, item: &WorkItem) -> Vec<JobMcp> {
    let auth = || [("Authorization".to_string(), format!("Bearer {token}"))].into_iter().collect();
    let mut out = vec![
        JobMcp { name: "senclaw-core".into(), url: format!("{daemon_base}/mcp/core"), headers: auth() },
        JobMcp { name: "senclaw-kanban".into(), url: format!("{daemon_base}/mcp/kanban"), headers: auth() },
    ];
    for spec in &item.mcp {
        if let McpServerSpec::Http { name, url } = spec {
            if !out.iter().any(|m| &m.name == name) {
                out.push(JobMcp { name: name.clone(), url: url.clone(), headers: Default::default() });
            }
        }
    }
    out
}

fn with_token(req: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    req.header(crate::apps::token::HEADER_APP_TOKEN, token)
        .header(app_space_sdk::HEADER_API_VERSION, app_space_sdk::API_VERSION.to_string())
}

/// Run `item` on `target` and return the outcome to finalize it with.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    ctx: &AppWorkerContext,
    target: &WorkerTarget,
    item: &WorkItem,
    assignee: &str,
    working_dir: &str,
    worktree: Option<&crate::worktree::WorktreeInfo>,
    timeout: Duration,
) -> Outcome {
    if !target.background {
        return Outcome::Blocked {
            reason: format!(
                "app '{}' declares worker '{assignee}' but is not a background app; app workers must run in the background",
                target.app_id
            ),
        };
    }
    if target.port == 0 {
        return Outcome::Failed { error: format!("app '{}' has no port to reach its worker on", target.app_id) };
    }
    // Without its access token the app cannot tell the daemon from any local
    // process — and neither can we tell the app from whatever holds its port.
    let Some(app_token) = target.token.as_deref() else {
        return Outcome::Blocked { reason: format!("app '{}' has no access token; reinstall it", target.app_id) };
    };
    // The chat SenClaw's tools act for is the daemon's choice, pinned in the
    // token: an app cannot widen it with `?chat=`.
    let chat = crate::mcp::bridge_servers::resolve_group(&ctx.db, None).map(|g| g.jid);
    let guard = TokenGuard(crate::mcp::http_bridge::mint_dispatch_token(chat, timeout + TOKEN_GRACE));
    drive(ctx, target, app_token, item, assignee, working_dir, worktree, timeout, &guard.0).await
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    ctx: &AppWorkerContext,
    target: &WorkerTarget,
    app_token: &str,
    item: &WorkItem,
    assignee: &str,
    working_dir: &str,
    worktree: Option<&crate::worktree::WorktreeInfo>,
    timeout: Duration,
    token: &str,
) -> Outcome {
    let job = Job {
        item_id: item.id.clone(),
        assignee: assignee.to_string(),
        prompt: item.prompt.clone(),
        working_dir: working_dir.to_string(),
        worktree: worktree.map(|w| JobWorktree { path: w.path.clone(), branch: w.branch.clone(), base: w.base.clone() }),
        mcp: job_mcp(&ctx.daemon_base, token, item),
        timeout_secs: timeout.as_secs(),
    };
    let app = &target.app_id;
    let started = with_token(ctx.http.post(format!("{}/runs", target.base_url)), app_token).json(&job).send().await;
    let run_id = match started {
        Ok(r) if r.status().is_success() => match r.json::<Started>().await {
            Ok(s) => s.run_id,
            Err(e) => return Outcome::Failed { error: format!("app '{app}' answered an unreadable start: {e}") },
        },
        Ok(r) => {
            let status = r.status();
            let body: serde_json::Value = r.json().await.unwrap_or_default();
            return Outcome::Blocked {
                reason: format!("app '{app}' refused the job ({status}): {}", body["error"].as_str().unwrap_or("")),
            };
        }
        Err(e) => return Outcome::Failed { error: format!("app '{app}' is not reachable: {e}") },
    };
    tracing::info!(item = %item.id, app = %app, run = %run_id, "[mcp-dispatch] handed to app worker");

    let deadline = Instant::now() + timeout + DEADLINE_GRACE;
    let status_url = format!("{}/runs/{run_id}", target.base_url);
    let mut unreachable = 0u32;
    let mut unreadable = 0u32;
    loop {
        if Instant::now() >= deadline {
            let _ = with_token(ctx.http.delete(&status_url), app_token).send().await;
            return Outcome::TimedOut;
        }
        tokio::time::sleep(POLL_EVERY).await;
        match with_token(ctx.http.get(&status_url), app_token).send().await {
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
                return Outcome::Failed { error: format!("app '{app}' lost run {run_id} (did it restart?)") };
            }
            Ok(r) if r.status().is_success() => {
                unreachable = 0;
                match r.json::<RunStatus>().await {
                    Ok(RunStatus { outcome: Some(o), .. }) => return o,
                    Ok(_) => unreadable = 0,
                    Err(e) => {
                        unreadable += 1;
                        if unreadable >= MAX_PROTOCOL_ERRORS {
                            return Outcome::Failed { error: format!("app '{app}' answers run status unreadably: {e}") };
                        }
                    }
                }
            }
            _ => {
                unreachable += 1;
                if unreachable >= MAX_UNREACHABLE {
                    return Outcome::Failed { error: format!("app '{app}' stopped answering about run {run_id}") };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_get_the_bridge_with_their_token_and_keep_http_source_servers() {
        let item = WorkItem {
            id: "1".into(),
            assignee: Some("claude-code".into()),
            prompt: "p".into(),
            guidance: None,
            mcp: vec![
                McpServerSpec::Stdio { name: "senclaw-kanban".into(), command: "senclaw".into(), args: vec![], env: Default::default() },
                McpServerSpec::Http { name: "review-queue".into(), url: "http://127.0.0.1:4900/mcp".into() },
            ],
            workspace: Default::default(),
            depends_on: vec![],
            priority: 0,
            timeout_secs: None,
        };
        let m = job_mcp("http://127.0.0.1:18788", "mdt_x", &item);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].url, "http://127.0.0.1:18788/mcp/core");
        assert_eq!(m[1].headers["Authorization"], "Bearer mdt_x");
        assert_eq!(m[2].name, "review-queue");
        assert!(m[2].headers.is_empty());
    }
}

//! sen-link MCP server — lets an agent (a SenClaw chat, or Claude Code through
//! the `/mcp/core` bridge) delegate work to a paired SenClaw and follow it.
//!
//! Naming per CLAUDE.md: server `senclaw-link`, tool prefix `link_`.
//! The tools are loopback HTTP calls to the daemon's `/api/sen-link/*`: the
//! peer store, hub channels and A2A client live in the daemon process, not in
//! this subprocess (same reasoning as `space_apps`).

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rmcp::ServiceExt;
use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct SendParams {
    /// Peer name or id (see `link_peers`).
    peer: String,
    /// What the other SenClaw should do. Self-contained: it does not see this chat.
    text: String,
    /// Ask the peer to work on its own git branch (worktree). Default true.
    #[serde(default)]
    worktree: Option<bool>,
    /// Follow-up on an existing task instead of a new one (answers a task
    /// waiting for input).
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct TaskParams {
    /// Peer name or id.
    peer: String,
    /// Task id returned by `link_send`.
    task_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct PeerParams {
    /// Peer name or id.
    peer: String,
}

#[derive(Clone)]
pub struct McpLinkServer {
    base_url: String,
    token: Option<String>,
    http: reqwest::Client,
}

/// A2A `Task` → the compact shape agents (and `schedule_watch`) read:
/// `state` is one word — submitted | working | input_required | completed |
/// failed | canceled.
pub fn compact_task(t: &Value) -> Value {
    let state = t["status"]["state"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches("TASK_STATE_")
        .to_ascii_lowercase();
    let text = |parts: &Value| -> String {
        parts
            .as_array()
            .map(|a| a.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    };
    let results: Vec<Value> = t["artifacts"]
        .as_array()
        .map(|a| a.iter().map(|x| json!({ "name": x["name"], "text": text(&x["parts"]) })).collect())
        .unwrap_or_default();
    json!({
        "task_id": t["id"],
        "state": state,
        "title": t["metadata"]["title"],
        "assignee": t["metadata"]["assignee"],
        "latest": text(&t["status"]["message"]["parts"]),
        "results": results,
    })
}

impl McpLinkServer {
    /// Build from `SENCLAW_LINK_API_URL`, or `None` when it is absent (see
    /// [`crate::mcp::wiki_server::McpWikiServer::from_env`]).
    pub fn from_env() -> Result<Option<Self>> {
        let Some(base_url) = std::env::var("SENCLAW_LINK_API_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
        else {
            return Ok(None);
        };
        let token = std::env::var("SENCLAW_API_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let http = reqwest::Client::builder().timeout(TIMEOUT).build().unwrap_or_default();
        Ok(Some(Self { base_url, token, http }))
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.http.request(method, format!("{}{}", self.base_url, path));
        if let Some(t) = &self.token {
            req = req.header("X-SenClaw-Token", t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.context("reach the SenClaw daemon")?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("{}", v["error"].as_str().unwrap_or(status.as_str()));
        }
        Ok(v)
    }

    /// A peer id from a name or id (exact id, then case-insensitive name).
    async fn peer_id(&self, peer: &str) -> Result<String> {
        let v = self.call(reqwest::Method::GET, "/api/sen-link/peers", None).await?;
        let peers = v["peers"].as_array().cloned().unwrap_or_default();
        let wanted = peer.trim();
        peers
            .iter()
            .find(|p| p["id"] == wanted)
            .or_else(|| peers.iter().find(|p| p["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(wanted))))
            .and_then(|p| p["id"].as_str().map(str::to_string))
            .ok_or_else(|| anyhow!("no paired SenClaw named `{wanted}` — see link_peers"))
    }

    async fn peers_impl(&self) -> Result<Value> {
        let v = self.call(reqwest::Method::GET, "/api/sen-link/peers", None).await?;
        let peers: Vec<Value> = v["peers"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|p| json!({ "id": p["id"], "name": p["name"], "can_send": p["canSend"], "via_hub": p["viaRelay"] }))
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({ "peers": peers }))
    }

    async fn send_impl(&self, p: SendParams) -> Result<Value> {
        if p.text.trim().is_empty() {
            bail!("text is empty");
        }
        let id = self.peer_id(&p.peer).await?;
        let v = self
            .call(
                reqwest::Method::POST,
                &format!("/api/sen-link/peers/{}/messages", urlencoding::encode(&id)),
                Some(json!({ "text": p.text, "worktree": p.worktree.unwrap_or(true), "taskId": p.task_id })),
            )
            .await?;
        Ok(compact_task(&v["task"]))
    }

    async fn task_impl(&self, p: TaskParams) -> Result<Value> {
        let id = self.peer_id(&p.peer).await?;
        let v = self
            .call(
                reqwest::Method::GET,
                &format!("/api/sen-link/peers/{}/tasks/{}", urlencoding::encode(&id), urlencoding::encode(&p.task_id)),
                None,
            )
            .await?;
        Ok(compact_task(&v))
    }

    async fn tasks_impl(&self, p: PeerParams) -> Result<Value> {
        let id = self.peer_id(&p.peer).await?;
        let v = self
            .call(reqwest::Method::GET, &format!("/api/sen-link/peers/{}/tasks", urlencoding::encode(&id)), None)
            .await?;
        let tasks: Vec<Value> = v["tasks"].as_array().map(|a| a.iter().map(compact_task).collect()).unwrap_or_default();
        Ok(json!({ "tasks": tasks }))
    }

    async fn cancel_impl(&self, p: TaskParams) -> Result<Value> {
        let id = self.peer_id(&p.peer).await?;
        let v = self
            .call(
                reqwest::Method::POST,
                &format!("/api/sen-link/peers/{}/tasks/{}/cancel", urlencoding::encode(&id), urlencoding::encode(&p.task_id)),
                None,
            )
            .await?;
        Ok(compact_task(&v))
    }
}

fn out(r: Result<Value>) -> String {
    match r {
        Ok(v) => v.to_string(),
        Err(e) => json!({ "error": e.to_string() }).to_string(),
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpLinkServer {
    #[rmcp::tool(description = "List the SenClaw instances this one is paired with over sen-link (A2A). \
                                 `can_send` false means the peer can send tasks here but cannot receive any.")]
    async fn link_peers(&self) -> String {
        out(self.peers_impl().await)
    }

    #[rmcp::tool(description = "Delegate a task to a paired SenClaw (sen-link / A2A). The text must stand on its own — \
        the peer does not see this conversation. Returns {task_id, state}. The peer's owner decides whether it \
        runs right away or waits for review. To be told when it finishes, arm schedule_watch with \
        tool `mcp__senclaw-link__link_task`, args {peer, task_id}, done_path `state`, done_op `in`, \
        done_values [completed, failed, canceled, input_required] — do not poll in this turn.")]
    async fn link_send(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<SendParams>,
    ) -> String {
        out(self.send_impl(p).await)
    }

    #[rmcp::tool(description = "State and results of a task delegated with link_send: state is submitted | working | \
        input_required | completed | failed | canceled; results carry the peer's summary and branch. \
        Answer input_required with link_send and task_id.")]
    async fn link_task(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<TaskParams>,
    ) -> String {
        out(self.task_impl(p).await)
    }

    #[rmcp::tool(description = "Every task this SenClaw has delegated to one peer, with its state.")]
    async fn link_tasks(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<PeerParams>,
    ) -> String {
        out(self.tasks_impl(p).await)
    }

    #[rmcp::tool(description = "Cancel a delegated task that has not started yet (a running one cannot be stopped from here).")]
    async fn link_cancel(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<TaskParams>,
    ) -> String {
        out(self.cancel_impl(p).await)
    }
}

/// Start the sen-link MCP server over stdio. Reads config from the env set by
/// [`crate::mcp::helper::link_mcp_config`].
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let server = McpLinkServer::from_env()?.context("SENCLAW_LINK_API_URL not set")?;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_compact_to_one_word_states_for_watches() {
        let t = json!({
            "id": "7",
            "status": { "state": "TASK_STATE_INPUT_REQUIRED", "message": { "parts": [{ "text": "need the bucket" }] } },
            "artifacts": [{ "artifactId": "a", "name": "complete", "parts": [{ "text": "done" }, { "text": "branch x" }] }],
            "metadata": { "title": "Upload", "assignee": "claude-code" }
        });
        let c = compact_task(&t);
        assert_eq!(c["state"], "input_required");
        assert_eq!(c["latest"], "need the bucket");
        assert_eq!(c["results"][0]["text"], "done\nbranch x");
        assert_eq!(c["task_id"], "7");
    }
}

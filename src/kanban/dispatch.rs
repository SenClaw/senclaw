//! Makes the Kanban board a dispatchable source: implements the SDK
//! `DispatchProvider` over `kanban.db`, so the core `MCPDispatcher` can claim
//! `Ready` tasks, run a worker agent per task, and report the outcome back.

use std::sync::Arc;

use app_space_sdk::dispatch::{
    Capacity, DispatchProvider, McpServerSpec, Outcome, WorkItem, Workspace,
};
use async_trait::async_trait;

use crate::kanban::api::now;
use crate::kanban::db::Db;

/// How long a claim is valid before the dispatcher may reclaim it (seconds).
const LEASE_SECS: i64 = 15 * 60;

pub struct KanbanDispatchProvider {
    db: Arc<Db>,
    /// The MCP server a worker gets so it can `kanban_show`/`complete`/`block`
    /// its own card. In-process the daemon passes a native `Stdio` spec
    /// (`senclaw kanban-server`); standalone it's an `Http` spec at this app's URL.
    worker_mcp: McpServerSpec,
}

impl KanbanDispatchProvider {
    pub fn new(db: Arc<Db>, worker_mcp: McpServerSpec) -> Self {
        Self { db, worker_mcp }
    }

    /// Convenience for the standalone binary: reach the app's HTTP MCP by URL.
    pub fn http(db: Arc<Db>, base_url: impl Into<String>) -> Self {
        Self {
            db,
            worker_mcp: McpServerSpec::Http {
                name: "senclaw-kanban".into(),
                url: base_url.into(),
            },
        }
    }

    fn guidance(card_id: i64) -> String {
        format!(
            "You are running Kanban task #{id}. First call the tool \
`mcp__senclaw-kanban__kanban_show` with card_id={id} to read its description, comments, and the \
summaries of the tasks it depends on. Do the work using your tools. You MUST finish with exactly \
one of: `mcp__senclaw-kanban__kanban_complete(card_id={id}, summary=…)` when the task is done, or \
`mcp__senclaw-kanban__kanban_block(card_id={id}, reason=…)` when you need human input or an \
external dependency. Never stop without calling one of them. For code-changing work, block with \
reason 'review-required: …' and attach details via `kanban_comment`.",
            id = card_id
        )
    }
}

#[async_trait]
impl DispatchProvider for KanbanDispatchProvider {
    async fn claim_ready(&self, cap: Capacity) -> anyhow::Result<Vec<WorkItem>> {
        let claimed = self
            .db
            .dispatch_claim(cap.total, cap.per_assignee, LEASE_SECS, now())?;
        let items = claimed
            .into_iter()
            .map(|c| {
                let mut prompt = c.title.clone();
                if !c.description.trim().is_empty() {
                    prompt.push_str("\n\n");
                    prompt.push_str(&c.description);
                }
                WorkItem {
                    id: c.id.to_string(),
                    assignee: c.assignee,
                    prompt,
                    guidance: Some(Self::guidance(c.id)),
                    mcp: vec![self.worker_mcp.clone()],
                    // A `worktree` label runs the card in its own git worktree
                    // (branch `senclaw/card-<id>`): the board's checkout is
                    // untouched until a person merges or opens a PR.
                    workspace: match &c.workspace_dir {
                        Some(dir) if !dir.trim().is_empty() => {
                            if c.labels.iter().any(|l| l.eq_ignore_ascii_case("worktree")) {
                                Workspace::Worktree { repo: dir.clone(), branch: Some(format!("senclaw/card-{}", c.id)) }
                            } else {
                                Workspace::Dir { path: dir.clone() }
                            }
                        }
                        _ => Workspace::Scratch,
                    },
                    depends_on: Vec::new(),
                    priority: prio_num(c.priority.as_deref()),
                    timeout_secs: None,
                }
            })
            .collect();
        Ok(items)
    }

    async fn heartbeat(&self, item_id: &str) -> anyhow::Result<()> {
        if let Ok(id) = item_id.parse::<i64>() {
            self.db.dispatch_heartbeat(id, LEASE_SECS, now())?;
        }
        Ok(())
    }

    async fn reclaim(&self) -> anyhow::Result<Vec<String>> {
        let ids = self.db.dispatch_reclaim(now())?;
        // Hermes-style promotion runs at the top of every dispatcher tick: todo
        // cards with all dependencies done move to Ready, where they get claimed.
        let promoted = self.db.dispatch_promote(now())?;
        if !promoted.is_empty() {
            tracing::info!(
                "[kanban] promoted {} task(s) todo→ready: {:?}",
                promoted.len(),
                promoted
            );
        }
        Ok(ids.into_iter().map(|i| i.to_string()).collect())
    }

    async fn finalize(&self, item_id: &str, outcome: Outcome) -> anyhow::Result<()> {
        let id: i64 = match item_id.parse() {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        // Always release the claim first.
        self.db.clear_claim(id)?;

        let (_t, _d, _col, board_id) = match self.db.card_detail(id) {
            Ok(v) => v,
            Err(_) => return Ok(()), // card gone
        };
        let role = self.db.card_role(id)?.unwrap_or_default();

        // The worker's own kanban_complete/block is authoritative; finalize only
        // reconciles a card the worker left unresolved.
        match outcome {
            Outcome::Completed { summary, metadata } => {
                // A worktree run always leaves a note with the branch, even when
                // the worker closed the card itself: the diff is what a person
                // reviews next, and the card is where they look for it.
                if let Some(wt) = metadata.get("worktree") {
                    let body = format!(
                        "worktree: {}\npath: {}\n{}",
                        wt["branch"].as_str().unwrap_or(""),
                        wt["path"].as_str().unwrap_or(""),
                        wt["summary"].as_str().unwrap_or("")
                    );
                    let _ = self.db.add_comment(id, "dispatcher", &body, "worktree", now());
                }
                if role != "done" {
                    if let Some(done) = self.db.column_by_role(board_id, "done")? {
                        self.db.move_card(id, done, 0, now())?;
                        let body = if summary.trim().is_empty() {
                            "auto-closed: agent returned without calling kanban_complete"
                                .to_string()
                        } else {
                            summary
                        };
                        let _ = self
                            .db
                            .add_comment(id, "dispatcher", &body, "complete", now());
                    }
                }
            }
            Outcome::Blocked { reason } => {
                if role != "blocked" {
                    if let Some(bl) = self.db.column_by_role(board_id, "blocked")? {
                        self.db.move_card(id, bl, 0, now())?;
                        let body = if reason.trim().is_empty() {
                            "blocked".to_string()
                        } else {
                            reason
                        };
                        let _ = self.db.add_comment(id, "dispatcher", &body, "block", now());
                    }
                }
            }
            Outcome::Failed { error } => {
                if role != "blocked" && role != "done" {
                    if let Some(bl) = self.db.column_by_role(board_id, "blocked")? {
                        self.db.move_card(id, bl, 0, now())?;
                        let _ = self.db.add_comment(
                            id,
                            "dispatcher",
                            &format!("gave_up: {error}"),
                            "block",
                            now(),
                        );
                    }
                }
            }
            Outcome::TimedOut => {
                if role != "blocked" && role != "done" {
                    if let Some(bl) = self.db.column_by_role(board_id, "blocked")? {
                        self.db.move_card(id, bl, 0, now())?;
                        let _ = self.db.add_comment(
                            id,
                            "dispatcher",
                            "gave_up: run timed out",
                            "block",
                            now(),
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

fn prio_num(p: Option<&str>) -> i32 {
    match p {
        Some("urgent") => 3,
        Some("high") => 2,
        Some("medium") => 1,
        _ => 0,
    }
}

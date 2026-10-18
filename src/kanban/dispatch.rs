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
use crate::kanban::projects::{effective_workspace_dir, ProjectInfo, SharedDirectory};

/// How long a claim is valid before the dispatcher may reclaim it (seconds).
const LEASE_SECS: i64 = 15 * 60;

pub struct KanbanDispatchProvider {
    db: Arc<Db>,
    /// The MCP server a worker gets so it can `kanban_show`/`complete`/`block`
    /// its own card. In-process the daemon passes a native `Stdio` spec
    /// (`senclaw kanban-server`); standalone it's an `Http` spec at this app's URL.
    worker_mcp: McpServerSpec,
    /// Where the projects of project boards are looked up. Without it every
    /// board is run as a board with no project.
    projects: Option<SharedDirectory>,
}

impl KanbanDispatchProvider {
    pub fn new(db: Arc<Db>, worker_mcp: McpServerSpec) -> Self {
        Self {
            db,
            worker_mcp,
            projects: None,
        }
    }

    /// Convenience for the standalone binary: reach the app's HTTP MCP by URL.
    pub fn http(db: Arc<Db>, base_url: impl Into<String>) -> Self {
        Self::new(
            db,
            McpServerSpec::Http {
                name: "senclaw-kanban".into(),
                url: base_url.into(),
            },
        )
    }

    /// Resolve the projects of project boards through `projects`.
    pub fn with_projects(mut self, projects: SharedDirectory) -> Self {
        self.projects = Some(projects);
        self
    }

    fn project_of(&self, project_id: Option<&str>) -> Option<ProjectInfo> {
        self.projects.as_deref()?.project(project_id?)
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
        let mut items = Vec::with_capacity(claimed.len());
        for c in claimed {
            let project = self.project_of(c.project_id.as_deref());
            if c.project_id.is_some() && project.is_none() {
                tracing::warn!(
                    "[kanban] card {} is on a board of a project that cannot be found; running it as a plain board",
                    c.id
                );
            }
            // A project's folder is checked when it is used, not only when the
            // project was made: running in some other folder instead would put
            // the card's work where nobody expects it.
            if let Some(p) = &project {
                if let Some(problem) = &p.root_problem {
                    let error = format!("project \"{}\" cannot be used: {problem}", p.name);
                    self.finalize(&c.id.to_string(), Outcome::Failed { error })
                        .await?;
                    continue;
                }
            }
            let mut prompt = c.title.clone();
            if !c.description.trim().is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&c.description);
            }
            // The worker's system prompt IS the guidance (a worker has no persona
            // of its own), so the project's context is appended to it, after the
            // card's own instructions.
            let mut guidance = Self::guidance(c.id);
            if let Some(block) = project.as_ref().and_then(|p| p.context_block.as_deref()) {
                guidance.push_str("\n\n");
                guidance.push_str(block);
            }
            items.push(WorkItem {
                id: c.id.to_string(),
                assignee: c.assignee,
                prompt,
                guidance: Some(guidance),
                mcp: vec![self.worker_mcp.clone()],
                // A `worktree` label runs the card in its own git worktree
                // (branch `senclaw/card-<id>`): the board's checkout
                // is untouched until a person merges or opens a PR.
                workspace: match effective_workspace_dir(
                    c.workspace_dir.as_deref(),
                    project.as_ref(),
                ) {
                    Some(dir) => {
                        if c.labels.iter().any(|l| l.eq_ignore_ascii_case("worktree")) {
                            Workspace::Worktree {
                                repo: dir,
                                branch: Some(format!("senclaw/card-{}", c.id)),
                            }
                        } else {
                            Workspace::Dir { path: dir }
                        }
                    }
                    None => Workspace::Scratch,
                },
                depends_on: Vec::new(),
                priority: prio_num(c.priority.as_deref()),
                timeout_secs: None,
            });
        }
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
                    let _ = self
                        .db
                        .add_comment(id, "dispatcher", &body, "worktree", now());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::projects::{sample_project, StaticDirectory};

    const P: &str = "prj_0a1b2c3d";

    struct Rig {
        db: Arc<Db>,
        _dir: tempfile::TempDir,
    }

    fn new_rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("kanban.db")).unwrap());
        Rig { db, _dir: dir }
    }

    fn worker_mcp() -> McpServerSpec {
        McpServerSpec::Stdio {
            name: "senclaw-kanban".into(),
            command: "senclaw".into(),
            args: vec!["kanban-server".into()],
            env: Default::default(),
        }
    }

    fn provider(rig: &Rig, projects: Vec<ProjectInfo>) -> KanbanDispatchProvider {
        KanbanDispatchProvider::new(Arc::clone(&rig.db), worker_mcp())
            .with_projects(Arc::new(StaticDirectory(projects)))
    }

    /// A standard board with one card in Ready; returns the card id.
    fn ready_card(
        rig: &Rig,
        workspace_dir: Option<&str>,
        project_id: Option<&str>,
        labels: Option<&str>,
    ) -> i64 {
        let board = rig
            .db
            .create_board_in("Board", "", true, workspace_dir, project_id, 1)
            .unwrap();
        let ready = rig.db.column_by_role(board, "ready").unwrap().unwrap();
        rig.db
            .add_card(
                ready,
                "Fix login",
                "Steps here",
                None,
                None,
                None,
                labels,
                None,
                1,
            )
            .unwrap()
    }

    fn cap() -> Capacity {
        Capacity {
            total: 5,
            per_assignee: 0,
        }
    }

    async fn claim_one(p: &KanbanDispatchProvider) -> WorkItem {
        let mut items = p.claim_ready(cap()).await.unwrap();
        assert_eq!(items.len(), 1, "one card was ready");
        items.remove(0)
    }

    fn root() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path())
            .unwrap()
            .to_string_lossy()
            .to_string();
        (dir, path)
    }

    #[tokio::test]
    async fn a_card_of_a_project_board_runs_in_the_root_with_the_project_after_its_instructions() {
        let rig = new_rig();
        let (_keep, root) = root();
        let project = sample_project(P, "Sen", &root);
        let block = project.context_block.clone().unwrap();
        let card = ready_card(&rig, None, Some(P), None);

        let item = claim_one(&provider(&rig, vec![project])).await;

        match &item.workspace {
            Workspace::Dir { path } => assert_eq!(path, &root),
            other => panic!("expected the project root, got {other:?}"),
        }
        let guidance = item.guidance.unwrap();
        assert_eq!(
            guidance,
            format!("{}\n\n{block}", KanbanDispatchProvider::guidance(card))
        );
        assert!(guidance.starts_with("You are running Kanban task #"));
        assert!(guidance.contains("PROJECT-CONTEXT-MARKER for Sen"));
        assert_eq!(item.prompt, "Fix login\n\nSteps here");
    }

    #[tokio::test]
    async fn a_board_without_a_project_is_claimed_exactly_as_it_always_was() {
        // With a directory attached and without one: the same item, field for field.
        let (_keep, root) = root();
        for attached in [true, false] {
            for (dir, labels) in [
                (None, None),
                (Some("/work/own"), None),
                (Some("/work/own"), Some(r#"["worktree"]"#)),
                (Some("  "), None),
            ] {
                let rig = new_rig();
                let card = ready_card(&rig, dir, None, labels);
                let mut p = KanbanDispatchProvider::new(Arc::clone(&rig.db), worker_mcp());
                if attached {
                    p = p.with_projects(Arc::new(StaticDirectory(vec![sample_project(
                        P, "Sen", &root,
                    )])));
                }
                let item = claim_one(&p).await;

                let workspace = match (dir.filter(|d| !d.trim().is_empty()), labels) {
                    (Some(d), Some(_)) => Workspace::Worktree {
                        repo: d.into(),
                        branch: Some(format!("senclaw/card-{card}")),
                    },
                    (Some(d), None) => Workspace::Dir { path: d.into() },
                    (None, _) => Workspace::Scratch,
                };
                let expected = WorkItem {
                    id: card.to_string(),
                    assignee: None,
                    prompt: "Fix login\n\nSteps here".into(),
                    guidance: Some(KanbanDispatchProvider::guidance(card)),
                    mcp: vec![worker_mcp()],
                    workspace,
                    depends_on: Vec::new(),
                    priority: 0,
                    timeout_secs: None,
                };
                assert_eq!(
                    serde_json::to_string(&item).unwrap(),
                    serde_json::to_string(&expected).unwrap(),
                    "attached={attached} dir={dir:?} labels={labels:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_folder_of_its_own_inside_the_root_is_kept_and_one_outside_gives_way_to_the_root() {
        let (_keep, root) = root();
        let inside = format!("{root}/packages/api");
        let rig = new_rig();
        ready_card(&rig, Some(&inside), Some(P), None);
        let p = provider(&rig, vec![sample_project(P, "Sen", &root)]);
        match claim_one(&p).await.workspace {
            Workspace::Dir { path } => assert_eq!(path, inside),
            other => panic!("{other:?}"),
        }

        let (_other_keep, elsewhere) = self::root();
        let rig = new_rig();
        ready_card(&rig, Some(&elsewhere), Some(P), None);
        let p = provider(&rig, vec![sample_project(P, "Sen", &root)]);
        match claim_one(&p).await.workspace {
            Workspace::Dir { path } => assert_eq!(path, root),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn the_worktree_label_isolates_the_card_in_a_worktree_of_the_root() {
        let (_keep, root) = root();
        let rig = new_rig();
        let card = ready_card(&rig, None, Some(P), Some(r#"["Worktree"]"#));
        let item = claim_one(&provider(&rig, vec![sample_project(P, "Sen", &root)])).await;
        match item.workspace {
            Workspace::Worktree { repo, branch } => {
                assert_eq!(repo, root);
                assert_eq!(branch, Some(format!("senclaw/card-{card}")));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_project_with_no_context_leaves_the_guidance_alone_and_an_archived_one_still_runs() {
        let (_keep, root) = root();
        let rig = new_rig();
        let card = ready_card(&rig, None, Some(P), None);
        let mut project = sample_project(P, "Sen", &root);
        project.context_block = None;
        project.archived = true;
        let item = claim_one(&provider(&rig, vec![project])).await;
        assert_eq!(item.guidance, Some(KanbanDispatchProvider::guidance(card)));
        assert!(matches!(item.workspace, Workspace::Dir { ref path } if path == &root));
    }

    #[tokio::test]
    async fn a_project_that_cannot_be_found_leaves_a_plain_board_to_run_as_one() {
        let rig = new_rig();
        let card = ready_card(&rig, Some("/work/own"), Some(P), None);
        let item = claim_one(&provider(&rig, Vec::new())).await;
        assert_eq!(item.guidance, Some(KanbanDispatchProvider::guidance(card)));
        assert!(matches!(item.workspace, Workspace::Dir { ref path } if path == "/work/own"));

        // No directory at all (the stdio process): the same.
        let rig = new_rig();
        ready_card(&rig, None, Some(P), None);
        let bare = KanbanDispatchProvider::new(Arc::clone(&rig.db), worker_mcp());
        assert!(matches!(
            claim_one(&bare).await.workspace,
            Workspace::Scratch
        ));
    }

    #[tokio::test]
    async fn a_root_that_cannot_be_used_blocks_the_card_instead_of_running_it_elsewhere() {
        let (_keep, root) = root();
        let rig = new_rig();
        let card = ready_card(&rig, None, Some(P), None);
        let mut project = sample_project(P, "Sen", &root);
        project.root_problem = Some("the folder is gone".into());

        let items = provider(&rig, vec![project])
            .claim_ready(cap())
            .await
            .unwrap();

        assert!(items.is_empty());
        assert_eq!(rig.db.card_role(card).unwrap().as_deref(), Some("blocked"));
        let notes = rig.db.comments_of_card(card).unwrap();
        let last = notes.last().unwrap();
        assert_eq!(last.kind, "block");
        assert!(last
            .body
            .starts_with("gave_up: project \"Sen\" cannot be used"));
        assert!(last.body.contains("the folder is gone"));
        // And the card is no longer claimed, so a person can move it back.
        assert!(rig
            .db
            .dispatch_claim(5, 0, LEASE_SECS, now())
            .unwrap()
            .is_empty());
    }
}

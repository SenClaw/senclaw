//! Makes the Kanban board a dispatchable source: implements the SDK
//! `DispatchProvider` over `kanban.db`, so the core `MCPDispatcher` can claim
//! `Ready` tasks, run a worker agent per task, and report the outcome back.

use std::sync::Arc;
use std::time::Duration;

use app_space_sdk::dispatch::{
    Capacity, DispatchProvider, McpServerSpec, Outcome, WorkItem, Workspace,
};
use async_trait::async_trait;
use futures::future::join_all;

use crate::kanban::api::now;
use crate::kanban::db::{ClaimedCard, Db};
use crate::kanban::projects::{effective_workspace_dir, ProjectInfo, SharedDirectory};
use crate::util::text::truncate_on_char_boundary;

/// How long a claim is valid before the dispatcher may reclaim it (seconds).
const LEASE_SECS: i64 = 15 * 60;

/// The longest a card waits for its project's facts. Past it the card runs
/// without them: remembering is never worth a card that does not start.
const RECALL_BUDGET: Duration = Duration::from_secs(3);

/// The comment author of the lines `finalize` writes itself.
const DISPATCHER: &str = "dispatcher";

/// What a fact retained from a finished card says it came from.
const RETAIN_SOURCE: &str = "kanban";

/// The most of a worker's summary handed to fact extraction (bytes).
const RETAIN_MAX_BYTES: usize = 4000;

pub struct KanbanDispatchProvider {
    db: Arc<Db>,
    /// The MCP server a worker gets so it can `kanban_show`/`complete`/`block`
    /// its own card. In-process the daemon passes a native `Stdio` spec
    /// (`senclaw kanban-server`); standalone it's an `Http` spec at this app's URL.
    worker_mcp: McpServerSpec,
    /// Where the projects of project boards are looked up. Without it every
    /// board is run as a board with no project.
    projects: Option<SharedDirectory>,
    /// How long `claim_ready` waits for a project's facts, per tick.
    recall_budget: Duration,
}

impl KanbanDispatchProvider {
    pub fn new(db: Arc<Db>, worker_mcp: McpServerSpec) -> Self {
        Self {
            db,
            worker_mcp,
            projects: None,
            recall_budget: RECALL_BUDGET,
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

    /// The project's facts that bear on `query`, as a block for the card's
    /// prompt. `None` — and the card runs as it would have — when the project
    /// has none, cannot be read, or does not answer within the budget. The
    /// lookup runs as a task of its own so that even a panic in it is only a
    /// line in the log.
    async fn recall_for(&self, query: &str, project: Option<&ProjectInfo>) -> Option<String> {
        let directory = Arc::clone(self.projects.as_ref()?);
        let id = project?.id.clone();
        let query = query.to_string();
        let mut lookup = {
            let id = id.clone();
            tokio::spawn(async move { directory.recall(&id, &query).await })
        };
        match tokio::time::timeout(self.recall_budget, &mut lookup).await {
            Ok(Ok(block)) => block.filter(|b| !b.trim().is_empty()),
            Ok(Err(e)) => {
                tracing::warn!("[kanban] recalling the facts of project {id} failed: {e}");
                None
            }
            Err(_) => {
                lookup.abort();
                tracing::warn!(
                    "[kanban] the facts of project {id} did not answer within {:?}; running the card without them",
                    self.recall_budget
                );
                None
            }
        }
    }

    /// Keeps what the worker said it achieved among the project's facts, so
    /// the next card of the project hears of it.
    ///
    /// Only a worker's own `kanban_complete` counts. A card the worker blocked,
    /// one it left for `finalize` to close (still not `done` here) and one it
    /// closed with no summary say nothing about what the work achieved. This
    /// can fail nothing: the model call is spawned and every error ends in the
    /// log, so `finalize` never waits for a model.
    fn retain_completion(&self, card_id: i64, title: &str, board_id: i64, role: &str) {
        if role != "done" {
            return;
        }
        let Some(directory) = self.projects.clone() else {
            return;
        };
        let project_id = match self.db.board_meta(board_id) {
            Ok(meta) => meta.and_then(|m| m.project_id),
            Err(e) => {
                tracing::warn!("[kanban] card {card_id}: cannot read its board: {e}");
                return;
            }
        };
        let Some(project_id) = project_id else {
            return;
        };
        let summary = match self.db.comments_of_card(card_id) {
            Ok(notes) => notes
                .into_iter()
                .rev()
                .find(|n| n.kind == "complete" && n.author != DISPATCHER)
                .map(|n| n.body.trim().to_string())
                .filter(|body| !body.is_empty()),
            Err(e) => {
                tracing::warn!("[kanban] card {card_id}: cannot read its comments: {e}");
                None
            }
        };
        let Some(summary) = summary else {
            return;
        };
        let kept = truncate_on_char_boundary(&summary, RETAIN_MAX_BYTES);
        let ellipsis = if kept.len() < summary.len() {
            "…"
        } else {
            ""
        };
        let text = format!(
            "Kanban task \"{}\" completed: {kept}{ellipsis}",
            title.trim()
        );
        tokio::spawn(async move {
            directory.retain(&project_id, &text, RETAIN_SOURCE).await;
        });
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
        let mut cards = Vec::with_capacity(claimed.len());
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
            cards.push((c, project));
        }
        // What the project already knows about each card, asked for all of
        // them at once so the wait is one budget however many were claimed.
        let tasks: Vec<String> = cards.iter().map(|(c, _)| task_text(c)).collect();
        let recalled = join_all(
            cards
                .iter()
                .zip(&tasks)
                .map(|((_, project), task)| self.recall_for(task, project.as_ref())),
        )
        .await;
        let mut items = Vec::with_capacity(cards.len());
        for (((c, project), task), recalled) in cards.into_iter().zip(tasks).zip(recalled) {
            // What the project knows leads the prompt, ahead of the task.
            let prompt = match recalled {
                Some(block) => format!("{block}\n\n{task}"),
                None => task,
            };
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

        let (title, _d, _col, board_id) = match self.db.card_detail(id) {
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
                        .add_comment(id, DISPATCHER, &body, "worktree", now());
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
                            .add_comment(id, DISPATCHER, &body, "complete", now());
                    }
                }
                self.retain_completion(id, &title, board_id, &role);
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
                        let _ = self.db.add_comment(id, DISPATCHER, &body, "block", now());
                    }
                }
            }
            Outcome::Failed { error } => {
                if role != "blocked" && role != "done" {
                    if let Some(bl) = self.db.column_by_role(board_id, "blocked")? {
                        self.db.move_card(id, bl, 0, now())?;
                        let _ = self.db.add_comment(
                            id,
                            DISPATCHER,
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
                            DISPATCHER,
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

/// The card as its worker is told it: the title, then the description.
fn task_text(c: &ClaimedCard) -> String {
    let mut text = c.title.clone();
    if !c.description.trim().is_empty() {
        text.push_str("\n\n");
        text.push_str(&c.description);
    }
    text
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
    use crate::kanban::projects::{sample_project, ProjectDirectory, StaticDirectory};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

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

    /// How the fake directory's recall behaves.
    #[derive(Clone, Copy)]
    enum Recall {
        Answers,
        Stalls,
        Panics,
    }

    type Retained = (String, String, String);

    /// A directory that tells a canned block, records what it is asked and
    /// what is kept in it, and can be made to fail.
    struct FakeFacts {
        projects: Vec<ProjectInfo>,
        block: Option<String>,
        recall: Recall,
        retain_stalls: bool,
        asked: Mutex<Vec<(String, String)>>,
        kept: mpsc::UnboundedSender<Retained>,
    }

    #[async_trait]
    impl ProjectDirectory for FakeFacts {
        fn project(&self, id: &str) -> Option<ProjectInfo> {
            self.projects.iter().find(|p| p.id == id).cloned()
        }

        async fn recall(&self, project_id: &str, query: &str) -> Option<String> {
            self.asked
                .lock()
                .unwrap()
                .push((project_id.into(), query.into()));
            match self.recall {
                Recall::Answers => self.block.clone(),
                Recall::Stalls => std::future::pending().await,
                Recall::Panics => panic!("the fact store is down"),
            }
        }

        async fn retain(&self, project_id: &str, text: &str, source: &str) {
            let _ = self
                .kept
                .send((project_id.into(), text.into(), source.into()));
            if self.retain_stalls {
                std::future::pending::<()>().await;
            }
        }
    }

    const FACT_BLOCK: &str = "<memory_context>\nThe gateway listens on 7421.\n</memory_context>";

    fn fake_facts(
        projects: Vec<ProjectInfo>,
        block: Option<&str>,
    ) -> (FakeFacts, mpsc::UnboundedReceiver<Retained>) {
        let (kept, rx) = mpsc::unbounded_channel();
        let fake = FakeFacts {
            projects,
            block: block.map(String::from),
            recall: Recall::Answers,
            retain_stalls: false,
            asked: Mutex::new(Vec::new()),
            kept,
        };
        (fake, rx)
    }

    fn provider_over(rig: &Rig, directory: Arc<FakeFacts>) -> KanbanDispatchProvider {
        KanbanDispatchProvider::new(Arc::clone(&rig.db), worker_mcp()).with_projects(directory)
    }

    /// What `kanban_complete` does when a worker calls it.
    fn worker_completes(rig: &Rig, card: i64, summary: &str) {
        let (_, _, _, board) = rig.db.card_detail(card).unwrap();
        let done = rig.db.column_by_role(board, "done").unwrap().unwrap();
        rig.db.move_card(card, done, 0, now()).unwrap();
        if !summary.is_empty() {
            rig.db
                .add_comment(card, "agent", summary, "complete", now())
                .unwrap();
        }
    }

    /// What `kanban_block` does.
    fn worker_blocks(rig: &Rig, card: i64, reason: &str) {
        let (_, _, _, board) = rig.db.card_detail(card).unwrap();
        let blocked = rig.db.column_by_role(board, "blocked").unwrap().unwrap();
        rig.db.move_card(card, blocked, 0, now()).unwrap();
        rig.db
            .add_comment(card, "agent", reason, "block", now())
            .unwrap();
    }

    fn ran_ok() -> Outcome {
        Outcome::Completed {
            summary: "Done.".into(),
            metadata: serde_json::json!({}),
        }
    }

    /// Whatever was handed to `retain` by now; the model call is spawned, so
    /// give the spawned task its turn first.
    async fn kept_by_now(rx: &mut mpsc::UnboundedReceiver<Retained>) -> Vec<Retained> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut out = Vec::new();
        while let Ok(r) = rx.try_recv() {
            out.push(r);
        }
        out
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

    #[tokio::test]
    async fn what_the_project_knows_leads_the_prompt_of_its_card() {
        let (_keep, root) = root();
        let rig = new_rig();
        ready_card(&rig, None, Some(P), None);
        let (fake, _rx) = fake_facts(vec![sample_project(P, "Sen", &root)], Some(FACT_BLOCK));
        let fake = Arc::new(fake);

        let item = claim_one(&provider_over(&rig, Arc::clone(&fake))).await;

        assert_eq!(
            item.prompt,
            format!("{FACT_BLOCK}\n\nFix login\n\nSteps here")
        );
        // The question is the task as the worker is told it, asked of the project the board is in.
        assert_eq!(
            *fake.asked.lock().unwrap(),
            vec![(P.to_string(), "Fix login\n\nSteps here".to_string())]
        );
        // The instructions and the folder are the ones a card without facts gets.
        let guidance = item.guidance.unwrap();
        assert!(guidance.starts_with("You are running Kanban task #"));
        assert!(guidance.contains("PROJECT-CONTEXT-MARKER for Sen"));
        assert!(matches!(item.workspace, Workspace::Dir { ref path } if path == &root));
    }

    #[tokio::test]
    async fn a_project_with_nothing_to_tell_claims_a_card_exactly_as_before() {
        let (_keep, root) = root();
        let with_none = {
            let rig = new_rig();
            ready_card(&rig, None, Some(P), None);
            let (fake, _rx) = fake_facts(vec![sample_project(P, "Sen", &root)], None);
            serde_json::to_string(&claim_one(&provider_over(&rig, Arc::new(fake))).await).unwrap()
        };
        let with_blank = {
            let rig = new_rig();
            ready_card(&rig, None, Some(P), None);
            let (fake, _rx) = fake_facts(vec![sample_project(P, "Sen", &root)], Some("  \n"));
            serde_json::to_string(&claim_one(&provider_over(&rig, Arc::new(fake))).await).unwrap()
        };
        let before = {
            let rig = new_rig();
            ready_card(&rig, None, Some(P), None);
            serde_json::to_string(
                &claim_one(&provider(&rig, vec![sample_project(P, "Sen", &root)])).await,
            )
            .unwrap()
        };
        assert_eq!(with_none, before);
        assert_eq!(with_blank, before);
    }

    #[tokio::test]
    async fn a_board_without_a_project_is_never_told_what_a_project_knows() {
        let (_keep, root) = root();
        let rig = new_rig();
        let card = ready_card(&rig, Some("/work/own"), None, None);
        let (fake, _rx) = fake_facts(vec![sample_project(P, "Sen", &root)], Some(FACT_BLOCK));
        let fake = Arc::new(fake);

        let item = claim_one(&provider_over(&rig, Arc::clone(&fake))).await;

        assert!(fake.asked.lock().unwrap().is_empty(), "nothing was asked");
        let expected = WorkItem {
            id: card.to_string(),
            assignee: None,
            prompt: "Fix login\n\nSteps here".into(),
            guidance: Some(KanbanDispatchProvider::guidance(card)),
            mcp: vec![worker_mcp()],
            workspace: Workspace::Dir {
                path: "/work/own".into(),
            },
            depends_on: Vec::new(),
            priority: 0,
            timeout_secs: None,
        };
        assert_eq!(
            serde_json::to_string(&item).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
    }

    #[tokio::test]
    async fn facts_that_do_not_answer_in_time_do_not_hold_the_card_back() {
        let (_keep, root) = root();
        let rig = new_rig();
        ready_card(&rig, None, Some(P), None);
        let (mut fake, _rx) = fake_facts(vec![sample_project(P, "Sen", &root)], Some(FACT_BLOCK));
        fake.recall = Recall::Stalls;
        let mut p = provider_over(&rig, Arc::new(fake));
        p.recall_budget = Duration::from_millis(100);

        let started = std::time::Instant::now();
        let item = claim_one(&p).await;

        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(item.prompt, "Fix login\n\nSteps here");
        assert!(matches!(item.workspace, Workspace::Dir { ref path } if path == &root));
    }

    #[tokio::test]
    async fn facts_that_fail_outright_are_a_log_line_and_the_card_still_runs() {
        let (_keep, root) = root();
        let rig = new_rig();
        ready_card(&rig, None, Some(P), None);
        let (mut fake, _rx) = fake_facts(vec![sample_project(P, "Sen", &root)], Some(FACT_BLOCK));
        fake.recall = Recall::Panics;

        let item = claim_one(&provider_over(&rig, Arc::new(fake))).await;

        assert_eq!(item.prompt, "Fix login\n\nSteps here");
    }

    #[tokio::test]
    async fn cards_claimed_together_wait_for_their_facts_together() {
        let (_keep, root) = root();
        let rig = new_rig();
        for _ in 0..3 {
            ready_card(&rig, None, Some(P), None);
        }
        let (mut fake, _rx) = fake_facts(vec![sample_project(P, "Sen", &root)], Some(FACT_BLOCK));
        fake.recall = Recall::Stalls;
        let mut p = provider_over(&rig, Arc::new(fake));
        p.recall_budget = Duration::from_millis(300);

        let started = std::time::Instant::now();
        let items = p.claim_ready(cap()).await.unwrap();

        assert_eq!(items.len(), 3);
        // One after the other this would be three budgets (900 ms).
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn what_a_worker_wrote_when_it_closed_the_card_is_kept_in_the_project() {
        let (_keep, root) = root();
        let rig = new_rig();
        let card = ready_card(&rig, None, Some(P), None);
        let (fake, mut rx) = fake_facts(vec![sample_project(P, "Sen", &root)], None);
        let p = provider_over(&rig, Arc::new(fake));
        claim_one(&p).await;

        worker_completes(
            &rig,
            card,
            "Moved login to cookie sessions; the suite passes.",
        );
        p.finalize(&card.to_string(), ran_ok()).await.unwrap();

        // Its own words, not the first line of what it answered last ("Done.").
        assert_eq!(
            kept_by_now(&mut rx).await,
            vec![(
                P.to_string(),
                "Kanban task \"Fix login\" completed: Moved login to cookie sessions; the suite passes."
                    .to_string(),
                "kanban".to_string()
            )]
        );
        assert_eq!(rig.db.card_role(card).unwrap().as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn a_card_that_was_not_closed_by_its_worker_keeps_nothing() {
        let (_keep, root) = root();
        for scenario in [
            "blocked",
            "blocked after an earlier run was completed",
            "closed without a summary",
            "left for the dispatcher, no words",
            "left for the dispatcher, some words",
        ] {
            let rig = new_rig();
            let card = ready_card(&rig, None, Some(P), None);
            let (fake, mut rx) = fake_facts(vec![sample_project(P, "Sen", &root)], None);
            let p = provider_over(&rig, Arc::new(fake));
            claim_one(&p).await;

            let summary = match scenario {
                "blocked" => {
                    worker_blocks(&rig, card, "needs the staging key");
                    "Done."
                }
                "blocked after an earlier run was completed" => {
                    // The card was done once, moved back, and blocks this time.
                    rig.db
                        .add_comment(card, "agent", "Last week's summary.", "complete", now())
                        .unwrap();
                    worker_blocks(&rig, card, "needs the staging key");
                    "Done."
                }
                "closed without a summary" => {
                    worker_completes(&rig, card, "");
                    "Done."
                }
                "left for the dispatcher, no words" => "",
                _ => "Which approach do you prefer?",
            };
            p.finalize(
                &card.to_string(),
                Outcome::Completed {
                    summary: summary.into(),
                    metadata: serde_json::json!({}),
                },
            )
            .await
            .unwrap();

            assert!(kept_by_now(&mut rx).await.is_empty(), "{scenario}");
        }
    }

    #[tokio::test]
    async fn a_card_that_failed_timed_out_or_was_blocked_keeps_nothing_even_after_a_summary() {
        let (_keep, root) = root();
        for outcome in [
            Outcome::Failed {
                error: "agent error".into(),
            },
            Outcome::TimedOut,
            Outcome::Blocked {
                reason: "needs a person".into(),
            },
        ] {
            let rig = new_rig();
            let card = ready_card(&rig, None, Some(P), None);
            let (fake, mut rx) = fake_facts(vec![sample_project(P, "Sen", &root)], None);
            let p = provider_over(&rig, Arc::new(fake));
            claim_one(&p).await;
            worker_completes(&rig, card, "Looked done to me.");

            p.finalize(&card.to_string(), outcome).await.unwrap();

            assert!(kept_by_now(&mut rx).await.is_empty());
        }
    }

    #[tokio::test]
    async fn a_card_of_a_board_without_a_project_keeps_nothing() {
        let rig = new_rig();
        let card = ready_card(&rig, None, None, None);
        let (fake, mut rx) = fake_facts(Vec::new(), None);
        let p = provider_over(&rig, Arc::new(fake));
        claim_one(&p).await;

        worker_completes(&rig, card, "Fixed it.");
        p.finalize(&card.to_string(), ran_ok()).await.unwrap();

        assert!(kept_by_now(&mut rx).await.is_empty());
    }

    #[tokio::test]
    async fn finishing_a_card_never_waits_for_the_facts_to_be_kept() {
        let (_keep, root) = root();
        let rig = new_rig();
        let card = ready_card(&rig, None, Some(P), None);
        let (mut fake, mut rx) = fake_facts(vec![sample_project(P, "Sen", &root)], None);
        fake.retain_stalls = true;
        let p = provider_over(&rig, Arc::new(fake));
        claim_one(&p).await;
        worker_completes(&rig, card, "Fixed it.");

        tokio::time::timeout(
            Duration::from_secs(5),
            p.finalize(&card.to_string(), ran_ok()),
        )
        .await
        .expect("finalize returned while keeping the fact was still pending")
        .unwrap();

        assert_eq!(kept_by_now(&mut rx).await.len(), 1, "it was handed over");
    }

    #[tokio::test]
    async fn a_very_long_summary_is_cut_before_it_is_handed_to_extraction() {
        let (_keep, root) = root();
        let rig = new_rig();
        let card = ready_card(&rig, None, Some(P), None);
        let (fake, mut rx) = fake_facts(vec![sample_project(P, "Sen", &root)], None);
        let p = provider_over(&rig, Arc::new(fake));
        claim_one(&p).await;
        // Multi-byte characters, so a cut inside one would panic.
        worker_completes(&rig, card, &"ế".repeat(RETAIN_MAX_BYTES));

        p.finalize(&card.to_string(), ran_ok()).await.unwrap();

        let kept = kept_by_now(&mut rx).await;
        let text = &kept[0].1;
        assert!(text.ends_with('…'));
        assert!(text.len() < RETAIN_MAX_BYTES + 64, "{} bytes", text.len());
    }

    #[tokio::test]
    async fn without_a_directory_finishing_a_card_is_what_it_always_was() {
        let rig = new_rig();
        let card = ready_card(&rig, None, Some(P), None);
        let bare = KanbanDispatchProvider::new(Arc::clone(&rig.db), worker_mcp());
        claim_one(&bare).await;
        worker_completes(&rig, card, "Fixed it.");

        bare.finalize(&card.to_string(), ran_ok()).await.unwrap();

        assert_eq!(rig.db.card_role(card).unwrap().as_deref(), Some("done"));
    }
}

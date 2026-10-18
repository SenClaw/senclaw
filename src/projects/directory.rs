//! The daemon's answer to the board store's questions about a project.
//!
//! The board store keeps its boards in a file of its own and never opens the
//! daemon's database or its fact store, so it asks through
//! [`ProjectDirectory`]. This is that directory over the registry: the row, the
//! root checked again for use, the `<project>` block rendered from the context
//! file as it is now, and the project's facts — read for a worker about to
//! start a card, written from the summary of a card it finished.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::Config;
use crate::db::Db;
use crate::kanban::{ProjectDirectory, ProjectInfo};
use crate::memory::engine::facts::{retain_text, FactTarget};
use crate::memory::engine::format::DETAIL_HINT;
use crate::memory::engine::{pre_retrieval, ArmSelection, RecallConfig, WRAPPER_TAG};

use super::context::render_block;
use super::facts::{worker_fact_target, worker_recall_query};
use super::root::{recheck_root, RootRules};
use super::types::is_valid_id;

/// A worker is told what its project's facts say and nothing else: it has no
/// profile, so no notes and no graph of the agent's.
const WORKER_ARMS: ArmSelection = ArmSelection {
    curated: false,
    cognitive: false,
    facts: true,
};

pub struct DbProjectDirectory {
    db: Arc<Db>,
    projects_dir: PathBuf,
    rules: RootRules,
    /// How many facts and how many words a recall may take, and which model
    /// extracts facts from what a worker wrote.
    config: Config,
}

impl DbProjectDirectory {
    pub fn new(db: Arc<Db>, projects_dir: PathBuf, rules: RootRules, config: Config) -> Self {
        Self {
            db,
            projects_dir,
            rules,
            config,
        }
    }

    /// Where a worker's summary is kept, read from the project as it is now so
    /// a change of `facts_write` holds from the next card. `None` for a project
    /// that keeps nothing of its workers, or that is gone.
    fn retain_target(&self, id: &str) -> Option<FactTarget> {
        if !is_valid_id(id) {
            return None;
        }
        match self.db.get_project(id) {
            Ok(project) => worker_fact_target(&project?),
            Err(e) => {
                tracing::warn!("[projects] cannot read project {id}: {e}");
                None
            }
        }
    }
}

/// The block a worker's prompt carries, from the text recall rendered, or
/// `None` when recall found nothing. The closing line that points at the
/// memory tools goes: a worker has none.
fn worker_block(rendered: &str) -> Option<String> {
    let body = rendered
        .strip_suffix(DETAIL_HINT)
        .unwrap_or(rendered)
        .trim_end();
    (!body.is_empty()).then(|| format!("<{WRAPPER_TAG}>\n{body}\n</{WRAPPER_TAG}>"))
}

#[async_trait]
impl ProjectDirectory for DbProjectDirectory {
    fn project(&self, id: &str) -> Option<ProjectInfo> {
        if !is_valid_id(id) {
            return None;
        }
        let project = match self.db.get_project(id) {
            Ok(p) => p?,
            Err(e) => {
                tracing::warn!("[projects] cannot read project {id}: {e}");
                return None;
            }
        };
        let (root, root_problem) = match recheck_root(&project.root, &self.rules) {
            Ok(canonical) => (canonical.to_string_lossy().to_string(), None),
            Err(problem) => (project.root.clone(), Some(problem.message)),
        };
        Some(ProjectInfo {
            context_block: render_block(&project, &self.projects_dir),
            archived: project.is_archived(),
            id: project.id,
            name: project.name,
            root,
            root_problem,
        })
    }

    async fn recall(&self, project_id: &str, query: &str) -> Option<String> {
        if !is_valid_id(project_id) {
            return None;
        }
        let question = worker_recall_query(
            project_id,
            query,
            self.config.memory.search_max_results as usize,
            chrono::Utc::now().timestamp(),
        );
        let rendered = pre_retrieval(
            WORKER_ARMS,
            &question,
            &RecallConfig {
                max_tokens: self.config.memory.engine_max_tokens as usize,
                ..Default::default()
            },
        )
        .await;
        worker_block(&rendered)
    }

    async fn retain(&self, project_id: &str, text: &str, source: &str) {
        let Some(target) = self.retain_target(project_id) else {
            tracing::debug!("[projects] {project_id} keeps no facts of its workers");
            return;
        };
        retain_text(&self.config, &target, text, source).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::memory::engine::facts::store::{FactKind, Inserted, NewFact};
    use crate::memory::engine::facts::{fact_arms, FactEngine};
    use crate::memory::engine::{format, recall, RecallArm};
    use crate::projects::store::{ArchivedFilter, NewProject};
    use crate::projects::types::FactsWrite;
    use crate::projects::{project_bank, write_context};

    struct Fixture {
        dir: DbProjectDirectory,
        db: Arc<Db>,
        home: tempfile::TempDir,
        projects: PathBuf,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let projects = home.path().join("data").join("projects");
        let db = Arc::new(Db::open_in_memory(&Config::from_env()).unwrap());
        let rules = RootRules {
            user_home: home.path().to_path_buf(),
            senclaw_home: home.path().join(".senclaw"),
            profiles_dir: home.path().join("data").join("profiles"),
            projects_dir: projects.clone(),
        };
        Fixture {
            dir: DbProjectDirectory::new(
                Arc::clone(&db),
                projects.clone(),
                rules,
                Config::from_env(),
            ),
            db,
            home,
            projects,
        }
    }

    fn make(f: &Fixture, name: &str) -> (String, PathBuf) {
        make_keeping(f, name, FactsWrite::Project)
    }

    fn make_keeping(f: &Fixture, name: &str, facts_write: FactsWrite) -> (String, PathBuf) {
        let root = f.home.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let p =
            f.db.create_project(
                NewProject {
                    name: name.into(),
                    root: root.to_string_lossy().to_string(),
                    description: String::new(),
                    facts_write,
                    inherit_common: true,
                },
                1,
            )
            .unwrap();
        (p.id, root)
    }

    #[test]
    fn a_project_is_looked_up_with_its_root_and_the_context_as_it_is_now() {
        let f = fixture();
        let (id, root) = make(&f, "alpha");
        let info = f.dir.project(&id).unwrap();
        assert_eq!(info.name, "alpha");
        assert_eq!(info.root, root.to_string_lossy());
        assert_eq!((info.root_problem, info.archived), (None, false));
        assert_eq!(info.context_block, None);

        write_context(&f.projects, &id, "Daemon Rust.", 0).unwrap();
        let block = f.dir.project(&id).unwrap().context_block.unwrap();
        assert!(block.starts_with("<project name=\"alpha\""));
        assert!(block.contains("Daemon Rust."));
    }

    #[test]
    fn archiving_shows_and_a_missing_or_malformed_id_finds_nothing() {
        let f = fixture();
        let (id, _) = make(&f, "alpha");
        f.db.update_project(
            &id,
            crate::projects::ProjectPatch {
                archived: Some(true),
                ..Default::default()
            },
            2,
        )
        .unwrap();
        assert!(f.dir.project(&id).unwrap().archived);
        assert!(f.dir.project("prj_00000000").is_none());
        assert!(f.dir.project("../etc").is_none());
        assert_eq!(f.db.list_projects(ArchivedFilter::All).unwrap().len(), 1);
    }

    #[test]
    fn a_root_that_vanished_is_reported_not_replaced() {
        let f = fixture();
        let (id, root) = make(&f, "alpha");
        std::fs::remove_dir_all(&root).unwrap();
        let info = f.dir.project(&id).unwrap();
        assert_eq!(info.root, root.to_string_lossy());
        assert!(info.root_problem.is_some());
    }

    // ===== the project's facts, as a kanban worker meets them =====

    const NOW: i64 = 1_000_000;

    fn engine_on(f: &Fixture) -> Arc<FactEngine> {
        Arc::new(FactEngine::new(Arc::clone(&f.db), None))
    }

    fn put(engine: &FactEngine, bank: &str, origin: &str, text: &str) {
        let r = engine
            .store
            .insert(
                bank,
                &NewFact {
                    text: text.into(),
                    kind: FactKind::World,
                    occurred: None,
                    mentioned_at: NOW,
                    entities: vec![],
                    embedding: None,
                    model: None,
                    source: "test".into(),
                    origin: origin.into(),
                    slot: None,
                },
            )
            .unwrap();
        assert!(matches!(r, Inserted::New(_)));
    }

    /// What a worker of `project_id` is told about `query` by `arms`: the
    /// question and the rendering are the directory's own, the arms are
    /// passed in because the daemon's fact store is a process-wide singleton.
    async fn told(arms: &[Arc<dyn RecallArm>], project_id: &str, query: &str) -> Option<String> {
        let question = worker_recall_query(project_id, query, 10, NOW);
        let recalled = recall(arms, &question, &RecallConfig::default()).await;
        worker_block(&format::render(&recalled.items))
    }

    #[tokio::test]
    async fn a_worker_is_told_the_facts_of_its_own_project_and_no_others() {
        let f = fixture();
        let (a, _) = make(&f, "alpha");
        let (b, _) = make(&f, "beta");
        let engine = engine_on(&f);
        put(
            &engine,
            &project_bank(&a),
            "",
            "The gateway of alpha listens on port 7421",
        );
        put(
            &engine,
            &project_bank(&b),
            "",
            "The gateway of beta listens on port 9090",
        );
        // The agent's own facts: common to its chats, and one chat's.
        put(
            &engine,
            "main",
            "",
            "The gateway of the office listens on port 5555",
        );
        put(
            &engine,
            "main",
            "web:x",
            "The gateway of my chat listens on port 6666",
        );
        let arms = fact_arms(Arc::clone(&engine));

        let block = told(&arms, &a, "Which port does the gateway listen on?")
            .await
            .unwrap();

        assert!(block.contains("7421"), "{block}");
        for other in ["9090", "5555", "6666"] {
            assert!(
                !block.contains(other),
                "{other} reached the worker: {block}"
            );
        }
        // The same question from the other project reads the other bank.
        let block = told(&arms, &b, "Which port does the gateway listen on?")
            .await
            .unwrap();
        assert!(block.contains("9090") && !block.contains("7421"), "{block}");
        // A project that knows nothing of it is told nothing, not the agent's facts.
        let (c, _) = make(&f, "gamma");
        assert_eq!(
            told(&arms, &c, "Which port does the gateway listen on?").await,
            None
        );
    }

    #[tokio::test]
    async fn an_archived_projects_facts_are_still_read() {
        let f = fixture();
        let (a, _) = make(&f, "alpha");
        let engine = engine_on(&f);
        put(
            &engine,
            &project_bank(&a),
            "",
            "The gateway of alpha listens on port 7421",
        );
        f.db.update_project(
            &a,
            crate::projects::ProjectPatch {
                archived: Some(true),
                ..Default::default()
            },
            2,
        )
        .unwrap();
        let arms = fact_arms(engine);

        // Recall takes an id and reads the bank: whether the project is
        // archived, or keeps nothing of its conversations, does not enter it.
        let block = told(&arms, &a, "Which port does the gateway listen on?")
            .await
            .unwrap();
        assert!(block.contains("7421"));
    }

    #[test]
    fn the_block_a_worker_gets_is_wrapped_and_points_at_no_tool_it_lacks() {
        use crate::memory::engine::types::Hit;
        let item = crate::memory::engine::Item {
            hit: Hit {
                arm: "facts",
                key: "k".into(),
                header: "The gateway listens on 7421".into(),
                body: String::new(),
                at: None,
                proof: 1,
            },
            score: 1.0,
            sources: vec![],
            tokens: 5,
        };
        let rendered = format::render(&[item]);
        assert!(rendered.ends_with(DETAIL_HINT));

        let block = worker_block(&rendered).unwrap();
        assert!(block.starts_with("<memory_context>\nMemory relevant to this request"));
        assert!(block.ends_with("\n</memory_context>"));
        assert!(block.contains("The gateway listens on 7421"));
        assert!(!block.contains("memory_recall"), "{block}");

        assert_eq!(worker_block(""), None);
    }

    #[tokio::test]
    async fn with_no_fact_store_up_a_worker_is_told_nothing() {
        let f = fixture();
        let (a, _) = make(&f, "alpha");
        assert_eq!(f.dir.recall(&a, "anything at all").await, None);
        assert_eq!(f.dir.recall("../etc", "anything").await, None);
    }

    #[test]
    fn what_a_worker_finishes_is_kept_only_where_the_project_keeps_its_facts() {
        let f = fixture();
        let (keeps, _) = make_keeping(&f, "keeps", FactsWrite::Project);
        let (chat, _) = make_keeping(&f, "chat", FactsWrite::Chat);
        let (off, _) = make_keeping(&f, "off", FactsWrite::Off);

        let t = f.dir.retain_target(&keeps).unwrap();
        assert_eq!(
            (t.bank.as_str(), t.origin.as_str()),
            (&*project_bank(&keeps), "")
        );
        assert_eq!(f.dir.retain_target(&chat), None);
        assert_eq!(f.dir.retain_target(&off), None);
        assert_eq!(f.dir.retain_target("prj_00000000"), None);
        assert_eq!(f.dir.retain_target("../etc"), None);

        // Read when the card finishes, not when the project was made.
        f.db.update_project(
            &chat,
            crate::projects::ProjectPatch {
                facts_write: Some(FactsWrite::Project),
                ..Default::default()
            },
            2,
        )
        .unwrap();
        assert!(f.dir.retain_target(&chat).is_some());
        // And an archived project keeps what its boards finish, like its chats do.
        f.db.update_project(
            &chat,
            crate::projects::ProjectPatch {
                archived: Some(true),
                ..Default::default()
            },
            3,
        )
        .unwrap();
        assert!(f.dir.retain_target(&chat).is_some());
    }
}

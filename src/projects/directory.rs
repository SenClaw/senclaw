//! The daemon's answer to the board store's "which project is this?".
//!
//! The board store keeps its boards in a file of its own and never opens the
//! daemon's database, so it asks through [`ProjectDirectory`]. This is that
//! directory over the registry: the row, the root checked again for use, and
//! the `<project>` block rendered from the context file as it is now.

use std::path::PathBuf;
use std::sync::Arc;

use crate::db::Db;
use crate::kanban::{ProjectDirectory, ProjectInfo};

use super::context::render_block;
use super::root::{recheck_root, RootRules};
use super::types::is_valid_id;

pub struct DbProjectDirectory {
    db: Arc<Db>,
    projects_dir: PathBuf,
    rules: RootRules,
}

impl DbProjectDirectory {
    pub fn new(db: Arc<Db>, projects_dir: PathBuf, rules: RootRules) -> Self {
        Self {
            db,
            projects_dir,
            rules,
        }
    }
}

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::projects::store::{ArchivedFilter, NewProject};
    use crate::projects::types::FactsWrite;
    use crate::projects::write_context;

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
            dir: DbProjectDirectory::new(Arc::clone(&db), projects.clone(), rules),
            db,
            home,
            projects,
        }
    }

    fn make(f: &Fixture, name: &str) -> (String, PathBuf) {
        let root = f.home.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let p =
            f.db.create_project(
                NewProject {
                    name: name.into(),
                    root: root.to_string_lossy().to_string(),
                    description: String::new(),
                    facts_write: FactsWrite::Project,
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
}

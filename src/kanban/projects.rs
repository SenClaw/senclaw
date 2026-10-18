//! What the board store knows about projects.
//!
//! A project lives in the daemon's own database. The board store is a separate
//! file that other processes (the stdio `kanban-server`) open too, so it never
//! opens the daemon's database: `run_daemon` hands it a [`ProjectDirectory`]
//! and that is the whole contract. A store without one still works — its boards
//! just have no project to resolve — which is what keeps this module standing
//! on its own.
//!
//! Where a board's workers run, in order: the board's own folder (which must
//! lie inside the project's root when there is a project), else the project's
//! root, else a scratch folder ([`effective_workspace_dir`]).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::kanban::db::{BoardMeta, Db};

/// A project as the board store needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    /// The project's root folder, absolute.
    pub root: String,
    /// Why the root cannot be used right now (the folder is gone, or is no
    /// longer one a project may have), or `None` when it can.
    pub root_problem: Option<String>,
    /// Archived projects keep the boards they have; they take no new ones.
    pub archived: bool,
    /// The `<project>` prompt block rendered from the project's context, or
    /// `None` when the project has none.
    pub context_block: Option<String>,
}

/// What the board store asks of the daemon about projects. Implemented over the
/// daemon's database and fact store; the board store only sees this trait, and
/// so never imports the fact store.
#[async_trait]
pub trait ProjectDirectory: Send + Sync {
    /// `None` when there is no such project (or it cannot be read).
    fn project(&self, id: &str) -> Option<ProjectInfo>;

    /// What the project's own facts say about `query`, as a block for the
    /// prompt of a worker about to start a card; `None` when they say nothing
    /// relevant or cannot be read. Never calls a model, and a failure is
    /// `None`, not an error: it must not hold a card back.
    async fn recall(&self, _project_id: &str, _query: &str) -> Option<String> {
        None
    }

    /// Keeps `text` among the project's facts, when the project keeps what its
    /// workers finish. Calls a model, so a caller that cannot wait spawns it;
    /// a failure is logged here and goes no further.
    async fn retain(&self, _project_id: &str, _text: &str, _source: &str) {}
}

pub type SharedDirectory = Arc<dyn ProjectDirectory>;

/// The folder a board's workers run in, or `None` for a scratch folder.
///
/// A board with a project never leaves its root: a folder of its own that is
/// not inside the root (the root was moved after the board was made) is
/// ignored in favour of the root.
pub fn effective_workspace_dir(
    board_dir: Option<&str>,
    project: Option<&ProjectInfo>,
) -> Option<String> {
    // Returned as stored: a board without a project must run exactly where it did.
    let own = board_dir.filter(|d| !d.trim().is_empty());
    match (own, project) {
        (Some(dir), Some(p)) if !path_within(&p.root, dir.trim()) => Some(p.root.clone()),
        (Some(dir), _) => Some(dir.to_string()),
        (None, Some(p)) => Some(p.root.clone()),
        (None, None) => None,
    }
}

/// Whether `dir` is `root` or a folder below it. `dir` must be absolute and
/// free of `..` (a `..` after a symlink leaves the place it appears to stay
/// in); symlinks in either path are resolved, and a part that does not exist
/// yet is compared as written.
pub fn path_within(root: &str, dir: &str) -> bool {
    let dir = Path::new(dir);
    if !dir.is_absolute() || dir.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    resolve_lenient(dir).starts_with(resolve_lenient(Path::new(root)))
}

/// `canonicalize`, except that a path which does not exist is resolved as far
/// as it does (macOS spells `/tmp` as `/private/tmp`).
fn resolve_lenient(p: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(c) = std::fs::canonicalize(&cur) {
            return tail.iter().rev().fold(c, |acc, part| acc.join(part));
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

fn view_with(meta: &BoardMeta, project: Option<&ProjectInfo>) -> Value {
    let mut v = serde_json::to_value(meta).unwrap_or_else(|_| json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "effective_workspace_dir".into(),
            json!(effective_workspace_dir(
                meta.workspace_dir.as_deref(),
                project
            )),
        );
    }
    v
}

/// A board as a client sees it: the stored row, plus where its workers run.
pub fn board_view(meta: &BoardMeta, directory: Option<&dyn ProjectDirectory>) -> Value {
    board_views(std::slice::from_ref(meta), directory)
        .pop()
        .unwrap_or(Value::Null)
}

/// [`board_view`] for several boards; each project is looked up once.
pub fn board_views(metas: &[BoardMeta], directory: Option<&dyn ProjectDirectory>) -> Vec<Value> {
    let mut found: HashMap<&str, Option<ProjectInfo>> = HashMap::new();
    metas
        .iter()
        .map(|meta| {
            let project = match (meta.project_id.as_deref(), directory) {
                (Some(id), Some(d)) => found.entry(id).or_insert_with(|| d.project(id)).as_ref(),
                _ => None,
            };
            view_with(meta, project)
        })
        .collect()
}

/// The board store as the project routes use it: the boards of a project, and
/// what becomes of them when the project is deleted.
#[derive(Clone)]
pub struct ProjectBoards {
    db: Arc<Db>,
    directory: Option<SharedDirectory>,
}

impl ProjectBoards {
    pub fn new(db: Arc<Db>, directory: Option<SharedDirectory>) -> Self {
        Self { db, directory }
    }

    /// The project's boards, as `GET /api/kanban/boards` shows them.
    pub fn list(&self, project_id: &str) -> anyhow::Result<Vec<Value>> {
        let metas = self.db.boards_of_project(project_id)?;
        Ok(board_views(&metas, self.directory.as_deref()))
    }

    /// The project is gone. Its boards stay and keep running where they ran:
    /// they leave the project, and those without a folder of their own are
    /// pinned to `root`. Returns how many boards changed.
    pub fn release(&self, project_id: &str, root: &str, now: i64) -> anyhow::Result<usize> {
        self.db.release_project(project_id, root, now)
    }
}

/// A directory over a fixed list of projects, for tests.
#[cfg(test)]
pub(crate) struct StaticDirectory(pub Vec<ProjectInfo>);

#[cfg(test)]
impl ProjectDirectory for StaticDirectory {
    fn project(&self, id: &str) -> Option<ProjectInfo> {
        self.0.iter().find(|p| p.id == id).cloned()
    }
}

/// A usable project with a context block, for tests.
#[cfg(test)]
pub(crate) fn sample_project(id: &str, name: &str, root: &str) -> ProjectInfo {
    ProjectInfo {
        id: id.into(),
        name: name.into(),
        root: root.into(),
        root_problem: None,
        archived: false,
        context_block: Some(format!(
            "<project name=\"{name}\" root=\"{root}\">\nPROJECT-CONTEXT-MARKER for {name}\n</project>"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(root: &str) -> ProjectInfo {
        ProjectInfo {
            id: "prj_0a1b2c3d".into(),
            name: "Sen".into(),
            root: root.into(),
            root_problem: None,
            archived: false,
            context_block: None,
        }
    }

    #[test]
    fn a_board_without_a_project_runs_where_it_always_did() {
        assert_eq!(effective_workspace_dir(None, None), None);
        assert_eq!(effective_workspace_dir(Some("  "), None), None);
        assert_eq!(
            effective_workspace_dir(Some("/work/own"), None),
            Some("/work/own".into())
        );
    }

    #[test]
    fn a_project_board_runs_in_the_root_unless_it_names_a_folder_inside_it() {
        let root = tempfile::tempdir().unwrap();
        let root_s = root.path().to_string_lossy().to_string();
        let p = info(&root_s);
        assert_eq!(
            effective_workspace_dir(None, Some(&p)),
            Some(root_s.clone())
        );
        let inside = root.path().join("packages").join("api");
        let inside_s = inside.to_string_lossy().to_string();
        assert_eq!(
            effective_workspace_dir(Some(&inside_s), Some(&p)),
            Some(inside_s)
        );
        // A folder the root no longer contains does not take the board out of it.
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(
            effective_workspace_dir(Some(&elsewhere.path().to_string_lossy()), Some(&p)),
            Some(root_s)
        );
    }

    #[test]
    fn containment_resolves_symlinks_and_refuses_parent_steps() {
        let root = tempfile::tempdir().unwrap();
        let root_s = root.path().to_string_lossy().to_string();
        assert!(path_within(&root_s, &root_s));
        assert!(path_within(&root_s, &format!("{root_s}/not/created/yet")));
        assert!(!path_within(&root_s, &format!("{root_s}/../elsewhere")));
        assert!(!path_within(&root_s, "relative/dir"));
        // A sibling that merely shares the prefix is outside.
        assert!(!path_within(&root_s, &format!("{root_s}-other")));
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
            assert!(!path_within(&root_s, &format!("{root_s}/link/deeper")));
        }
    }
}

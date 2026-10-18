//! SQL for the `projects` table, as methods on [`Db`].
//!
//! The store takes already-checked input from its caller where it needs the
//! filesystem (a root is canonical and allowed, see [`super::root`]) and
//! checks for itself what only the database can know: that a root is used by
//! no other project, and that a name is not blank.

use rusqlite::{params, OptionalExtension, Row};

use crate::db::Db;

use super::types::{FactsWrite, Project};

pub const NAME_MAX_CHARS: usize = 120;
pub const DESCRIPTION_MAX_CHARS: usize = 280;

#[derive(Debug, thiserror::Error)]
pub enum ProjectStoreError {
    /// Another project already has this root.
    #[error("project root already used by {name} ({id})")]
    RootInUse { id: String, name: String },
    #[error("no such project")]
    NotFound,
    #[error("{0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

impl From<rusqlite::Error> for ProjectStoreError {
    fn from(e: rusqlite::Error) -> Self {
        ProjectStoreError::Db(e.into())
    }
}

#[derive(Debug, Clone)]
pub struct NewProject {
    pub name: String,
    /// Canonical and allowed (see [`super::root::check_root`]).
    pub root: String,
    pub description: String,
    pub facts_write: FactsWrite,
    pub inherit_common: bool,
}

/// What to change; `None` leaves a field alone.
#[derive(Debug, Clone, Default)]
pub struct ProjectPatch {
    pub name: Option<String>,
    /// Canonical and allowed, as in [`NewProject::root`].
    pub root: Option<String>,
    pub description: Option<String>,
    pub facts_write: Option<FactsWrite>,
    pub inherit_common: Option<bool>,
    pub archived: Option<bool>,
}

/// Which projects a listing shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchivedFilter {
    /// Not archived (the default view).
    Active,
    Archived,
    All,
}

fn row_to_project(r: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get("id")?,
        name: r.get("name")?,
        root: r.get("root")?,
        description: r.get("description")?,
        // The column has a CHECK; a value outside it cannot be stored.
        facts_write: FactsWrite::parse(&r.get::<_, String>("facts_write")?)
            .unwrap_or(FactsWrite::Project),
        inherit_common: r.get::<_, i64>("inherit_common")? != 0,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        archived_at: r.get("archived_at")?,
    })
}

fn clean_name(name: &str) -> Result<String, ProjectStoreError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ProjectStoreError::Invalid("name is required"));
    }
    if name.chars().count() > NAME_MAX_CHARS {
        return Err(ProjectStoreError::Invalid("name is too long"));
    }
    Ok(name.to_string())
}

fn clean_description(text: &str) -> Result<String, ProjectStoreError> {
    let text = text.trim();
    if text.chars().count() > DESCRIPTION_MAX_CHARS {
        return Err(ProjectStoreError::Invalid("description is too long"));
    }
    Ok(text.to_string())
}

fn new_id() -> String {
    format!("prj_{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

impl Db {
    pub fn create_project(
        &self,
        new: NewProject,
        now_ms: i64,
    ) -> Result<Project, ProjectStoreError> {
        let name = clean_name(&new.name)?;
        let description = clean_description(&new.description)?;
        self.with_conn(|c| {
            if let Some(owner) = project_by_root(c, &new.root)? {
                return Ok(Err(ProjectStoreError::RootInUse {
                    id: owner.id,
                    name: owner.name,
                }));
            }
            // Eight hex digits collide about once in four billion; one retry
            // loop is cheaper than explaining a failed create.
            for _ in 0..8 {
                let id = new_id();
                let inserted = c.execute(
                    "INSERT OR IGNORE INTO projects
                       (id, name, root, description, facts_write, inherit_common,
                        created_at, updated_at, archived_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?7,NULL)",
                    params![
                        id,
                        name,
                        new.root,
                        description,
                        new.facts_write.as_str(),
                        new.inherit_common as i64,
                        now_ms,
                    ],
                )?;
                if inserted == 1 {
                    return Ok(Ok(Project {
                        id,
                        name,
                        root: new.root.clone(),
                        description,
                        facts_write: new.facts_write,
                        inherit_common: new.inherit_common,
                        created_at: now_ms,
                        updated_at: now_ms,
                        archived_at: None,
                    }));
                }
            }
            Err(anyhow::anyhow!("could not find a free project id"))
        })?
    }

    pub fn get_project(&self, id: &str) -> anyhow::Result<Option<Project>> {
        self.with_conn(|c| Ok(project_by_id(c, id)?))
    }

    pub fn find_project_by_root(&self, root: &str) -> anyhow::Result<Option<Project>> {
        self.with_conn(|c| Ok(project_by_root(c, root)?))
    }

    pub fn list_projects(&self, filter: ArchivedFilter) -> anyhow::Result<Vec<Project>> {
        self.with_conn(|c| {
            let sql = match filter {
                ArchivedFilter::Active => "SELECT * FROM projects WHERE archived_at IS NULL",
                ArchivedFilter::Archived => "SELECT * FROM projects WHERE archived_at IS NOT NULL",
                ArchivedFilter::All => "SELECT * FROM projects",
            };
            let mut stmt = c.prepare(&format!("{sql} ORDER BY lower(name), id"))?;
            let rows = stmt
                .query_map([], row_to_project)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn update_project(
        &self,
        id: &str,
        patch: ProjectPatch,
        now_ms: i64,
    ) -> Result<Project, ProjectStoreError> {
        self.with_conn(|c| {
            let Some(mut p) = project_by_id(c, id)? else {
                return Ok(Err(ProjectStoreError::NotFound));
            };
            if let Some(name) = &patch.name {
                match clean_name(name) {
                    Ok(n) => p.name = n,
                    Err(e) => return Ok(Err(e)),
                }
            }
            if let Some(text) = &patch.description {
                match clean_description(text) {
                    Ok(t) => p.description = t,
                    Err(e) => return Ok(Err(e)),
                }
            }
            if let Some(root) = &patch.root {
                if let Some(owner) = project_by_root(c, root)? {
                    if owner.id != p.id {
                        return Ok(Err(ProjectStoreError::RootInUse {
                            id: owner.id,
                            name: owner.name,
                        }));
                    }
                }
                p.root = root.clone();
            }
            if let Some(w) = patch.facts_write {
                p.facts_write = w;
            }
            if let Some(v) = patch.inherit_common {
                p.inherit_common = v;
            }
            match patch.archived {
                Some(true) if p.archived_at.is_none() => p.archived_at = Some(now_ms),
                Some(false) => p.archived_at = None,
                _ => {}
            }
            p.updated_at = now_ms;
            c.execute(
                "UPDATE projects SET name=?2, root=?3, description=?4, facts_write=?5,
                   inherit_common=?6, updated_at=?7, archived_at=?8 WHERE id=?1",
                params![
                    p.id,
                    p.name,
                    p.root,
                    p.description,
                    p.facts_write.as_str(),
                    p.inherit_common as i64,
                    p.updated_at,
                    p.archived_at,
                ],
            )?;
            Ok(Ok(p))
        })?
    }

    /// Remove the project and detach its sessions, in one transaction. The
    /// sessions keep their history; they just stop belonging to a project.
    /// Returns the sessions that were detached.
    pub fn delete_project(&self, id: &str) -> Result<Vec<DetachedChat>, ProjectStoreError> {
        self.with_conn_mut(|c| {
            let tx = c.transaction()?;
            if project_by_id(&tx, id)?.is_none() {
                return Ok(Err(ProjectStoreError::NotFound));
            }
            let chats = {
                let mut stmt = tx.prepare(
                    "SELECT jid, folder FROM groups WHERE project_id = ?1 ORDER BY added_at, jid",
                )?;
                let rows = stmt
                    .query_map(params![id], |r| {
                        Ok(DetachedChat {
                            jid: r.get(0)?,
                            folder: r.get(1)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            tx.execute(
                "UPDATE groups SET project_id = NULL WHERE project_id = ?1",
                params![id],
            )?;
            tx.execute("DELETE FROM projects WHERE id = ?1", params![id])?;
            tx.commit()?;
            Ok(Ok(chats))
        })?
    }
}

/// A session that belonged to a deleted project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedChat {
    pub jid: String,
    pub folder: String,
}

fn project_by_id(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Project>> {
    c.query_row(
        "SELECT * FROM projects WHERE id = ?1",
        params![id],
        row_to_project,
    )
    .optional()
}

fn project_by_root(c: &rusqlite::Connection, root: &str) -> rusqlite::Result<Option<Project>> {
    c.query_row(
        "SELECT * FROM projects WHERE root = ?1",
        params![root],
        row_to_project,
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::types::GroupBinding;

    fn db() -> Db {
        Db::open_in_memory(&Config::from_env()).unwrap()
    }

    fn new(name: &str, root: &str) -> NewProject {
        NewProject {
            name: name.into(),
            root: root.into(),
            description: String::new(),
            facts_write: FactsWrite::Project,
            inherit_common: true,
        }
    }

    fn session(jid: &str, project: Option<&str>) -> GroupBinding {
        GroupBinding {
            jid: jid.into(),
            folder: "main".into(),
            name: jid.into(),
            channel: String::new(),
            group_type: "chat".into(),
            requires_trigger: false,
            allowed_tools: None,
            allowed_paths: None,
            allowed_work_dirs: None,
            bot_token: None,
            max_messages: None,
            llm_config_id: None,
            last_active: None,
            added_at: format!("2026-10-18T00:00:0{}Z", jid.len() % 10),
            project_id: project.map(String::from),
        }
    }

    #[test]
    fn a_created_project_reads_back_with_defaults_and_a_prj_id() {
        let db = db();
        let p = db
            .create_project(new("  Sen  ", "/work/sen"), 1_000)
            .unwrap();
        assert!(super::super::types::is_valid_id(&p.id), "{}", p.id);
        assert_eq!((p.name.as_str(), p.root.as_str()), ("Sen", "/work/sen"));
        assert_eq!(
            (p.facts_write, p.inherit_common),
            (FactsWrite::Project, true)
        );
        assert_eq!(
            (p.created_at, p.updated_at, p.archived_at),
            (1_000, 1_000, None)
        );
        assert_eq!(db.get_project(&p.id).unwrap(), Some(p.clone()));
        assert_eq!(db.find_project_by_root("/work/sen").unwrap(), Some(p));
        assert_eq!(db.get_project("prj_00000000").unwrap(), None);
    }

    #[test]
    fn two_projects_cannot_share_a_root_and_the_error_names_the_owner() {
        let db = db();
        let first = db.create_project(new("First", "/work/a"), 1).unwrap();
        match db.create_project(new("Second", "/work/a"), 2) {
            Err(ProjectStoreError::RootInUse { id, name }) => {
                assert_eq!((id, name), (first.id.clone(), "First".to_string()));
            }
            other => panic!("{other:?}"),
        }
        // Nested roots are different roots.
        db.create_project(new("Nested", "/work/a/sub"), 3).unwrap();
        // Moving a project onto another's root is refused; onto its own is a no-op.
        let other = db.create_project(new("Other", "/work/b"), 4).unwrap();
        let clash = ProjectPatch {
            root: Some("/work/a".into()),
            ..Default::default()
        };
        assert!(matches!(
            db.update_project(&other.id, clash, 5),
            Err(ProjectStoreError::RootInUse { .. })
        ));
        let same = ProjectPatch {
            root: Some("/work/b".into()),
            ..Default::default()
        };
        db.update_project(&other.id, same, 5).unwrap();
    }

    #[test]
    fn names_and_descriptions_are_checked_by_the_store_itself() {
        let db = db();
        assert!(matches!(
            db.create_project(new("   ", "/a"), 1),
            Err(ProjectStoreError::Invalid(_))
        ));
        let long = "x".repeat(NAME_MAX_CHARS + 1);
        assert!(matches!(
            db.create_project(new(&long, "/a"), 1),
            Err(ProjectStoreError::Invalid(_))
        ));
        let mut n = new("ok", "/a");
        n.description = "y".repeat(DESCRIPTION_MAX_CHARS + 1);
        assert!(matches!(
            db.create_project(n, 1),
            Err(ProjectStoreError::Invalid(_))
        ));
        assert!(
            db.list_projects(ArchivedFilter::All).unwrap().is_empty(),
            "nothing half-created"
        );
    }

    #[test]
    fn a_patch_changes_only_what_it_names_and_stamps_updated_at() {
        let db = db();
        let p = db.create_project(new("Sen", "/work/sen"), 10).unwrap();
        let patch = ProjectPatch {
            description: Some("daemon".into()),
            facts_write: Some(FactsWrite::Off),
            inherit_common: Some(false),
            ..Default::default()
        };
        let u = db.update_project(&p.id, patch, 20).unwrap();
        assert_eq!(
            (u.name.as_str(), u.root.as_str(), u.description.as_str()),
            ("Sen", "/work/sen", "daemon")
        );
        assert_eq!((u.facts_write, u.inherit_common), (FactsWrite::Off, false));
        assert_eq!((u.created_at, u.updated_at), (10, 20));
        assert_eq!(db.get_project(&p.id).unwrap(), Some(u));
        assert!(matches!(
            db.update_project("prj_00000000", ProjectPatch::default(), 1),
            Err(ProjectStoreError::NotFound)
        ));
    }

    #[test]
    fn archiving_hides_a_project_from_the_default_list_and_is_undoable() {
        let db = db();
        let a = db.create_project(new("alpha", "/a"), 1).unwrap();
        let b = db.create_project(new("Beta", "/b"), 1).unwrap();
        let archive = ProjectPatch {
            archived: Some(true),
            ..Default::default()
        };
        let archived = db.update_project(&a.id, archive.clone(), 50).unwrap();
        assert_eq!(archived.archived_at, Some(50));
        // Archiving again keeps the first time.
        assert_eq!(
            db.update_project(&a.id, archive, 99).unwrap().archived_at,
            Some(50)
        );

        let names = |f| {
            db.list_projects(f)
                .unwrap()
                .into_iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(ArchivedFilter::Active), ["Beta"]);
        assert_eq!(names(ArchivedFilter::Archived), ["alpha"]);
        assert_eq!(
            names(ArchivedFilter::All),
            ["alpha", "Beta"],
            "case-insensitive by name"
        );

        let restored = db
            .update_project(
                &a.id,
                ProjectPatch {
                    archived: Some(false),
                    ..Default::default()
                },
                60,
            )
            .unwrap();
        assert_eq!(restored.archived_at, None);
        let _ = b;
    }

    #[test]
    fn deleting_a_project_detaches_its_sessions_and_keeps_them() {
        let db = db();
        let p = db.create_project(new("Sen", "/work/sen"), 1).unwrap();
        let q = db.create_project(new("Other", "/work/other"), 1).unwrap();
        for (jid, project) in [
            ("web:a", Some(p.id.as_str())),
            ("web:b", Some(p.id.as_str())),
            ("web:c", Some(q.id.as_str())),
            ("web:d", None),
        ] {
            db.upsert_group(&session(jid, project)).unwrap();
        }
        let detached = db.delete_project(&p.id).unwrap();
        let jids: Vec<&str> = detached.iter().map(|c| c.jid.as_str()).collect();
        assert_eq!(jids.len(), 2);
        assert!(jids.contains(&"web:a") && jids.contains(&"web:b"));
        assert!(detached.iter().all(|c| c.folder == "main"));
        assert_eq!(db.get_project(&p.id).unwrap(), None);
        for jid in ["web:a", "web:b"] {
            let g = db
                .get_group(jid)
                .unwrap()
                .expect("the session itself stays");
            assert_eq!(g.project_id, None, "{jid}");
        }
        assert_eq!(
            db.get_group("web:c")
                .unwrap()
                .unwrap()
                .project_id
                .as_deref(),
            Some(q.id.as_str())
        );
        assert!(matches!(
            db.delete_project(&p.id),
            Err(ProjectStoreError::NotFound)
        ));
        // The root is free again.
        db.create_project(new("Again", "/work/sen"), 2).unwrap();
    }
}

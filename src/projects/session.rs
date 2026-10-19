//! What a session needs from the project it was created in.
//!
//! Two things have to be decided identically by the agent pool, the HTTP
//! bridge and the tools that read the result, so they live here once:
//!
//! - **Which file holds the session's working folder** ([`workspace_state_key`]).
//!   For a session outside a project that is the agent profile's file
//!   (`workspace-state-<folder>.json`), shared by every session of the
//!   profile — which is how two code sessions of one profile came to see each
//!   other's folder. A session in a project gets a file of its own.
//! - **How a child process learns the project** ([`ENV_PROJECT_ROOT`],
//!   [`ENV_PROJECT_BANK`]): the MCP servers are separate processes started
//!   once per session, so the project is put in their environment at that
//!   moment, never looked up from the database later.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::types::GroupBinding;

use super::types::Project;

/// The project's root folder, for a session inside a project. The dispatch and
/// virtual-agent servers use it before any workspace-state file.
pub const ENV_PROJECT_ROOT: &str = "SENCLAW_PROJECT_ROOT";

/// The project's fact bank (`project:<id>`).
pub const ENV_PROJECT_BANK: &str = "SENCLAW_PROJECT_BANK";

/// The fact bank of a project.
pub fn project_bank(project_id: &str) -> String {
    format!("project:{project_id}")
}

/// The stem of the workspace-state file of `binding`:
/// `<senclaw home>/workspace-state-<key>.json`.
///
/// A pure function of the binding, so every place that reads or writes the
/// file arrives at the same name. Outside a project the key is the profile
/// folder, exactly as before. Inside one it adds a short hash of the jid —
/// the jid has colons and is long, the hash is neither.
pub fn workspace_state_key(binding: &GroupBinding) -> String {
    if binding.project_id.is_none() {
        return binding.folder.clone();
    }
    session_state_key(&binding.folder, &binding.jid)
}

/// The key of the file one session of a project owns.
pub fn session_state_key(folder: &str, jid: &str) -> String {
    let digest = Sha256::digest(jid.as_bytes());
    format!("{folder}@{}", &hex::encode(digest)[..12])
}

pub fn workspace_state_file_name(key: &str) -> String {
    format!("workspace-state-{key}.json")
}

/// Delete the workspace-state file one session of a project owns. Callers
/// pass only sessions that were in a project: outside one the file is the
/// profile's, shared with the profile's other sessions, and must stay.
pub fn remove_session_state_file(senclaw_home: &Path, folder: &str, jid: &str) {
    let path = senclaw_home.join(workspace_state_file_name(&session_state_key(folder, jid)));
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("[projects] could not remove {}: {e}", path.display()),
    }
}

/// The project `binding` was created in. `Ok(None)` for a session outside a
/// project, and also when the id names nothing (the row is gone) — the caller
/// decides how loudly to treat a dangling id.
pub fn project_for_binding(db: &Db, binding: &GroupBinding) -> anyhow::Result<Option<Project>> {
    match binding.project_id.as_deref() {
        Some(id) => db.get_project(id),
        None => Ok(None),
    }
}

// ===== Membership =====

/// Why a session cannot be put in, or moved in or out of, a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipError {
    /// The session already exists: its project is fixed at creation. A core
    /// is built once from the project (folder, environment, state file), and
    /// nothing rebuilds it when the project changes.
    Fixed,
    /// Only web-only sessions (`channel == ""`) join a project: a channel chat
    /// can have several people in it, and a project's context and facts are
    /// the owner's.
    Channel,
    /// Only `chat` and `code` sessions join a project.
    GroupType,
    UnknownProject,
    Archived,
    /// `projectId` was not a string.
    Malformed,
}

impl MembershipError {
    pub fn message(self) -> &'static str {
        match self {
            MembershipError::Fixed => {
                "projectId cannot be changed or removed once the session exists; create a new session in the project instead"
            }
            MembershipError::Channel => "only web-only sessions (empty channel) can belong to a project",
            MembershipError::GroupType => "only chat and code sessions can belong to a project",
            MembershipError::UnknownProject => "no such project",
            MembershipError::Archived => "the project is archived and takes no new sessions",
            MembershipError::Malformed => "projectId must be a string",
        }
    }
}

/// The `projectId` of a client message: `None` when the key is absent,
/// `Some(None)` when it is null or blank (asking for no project), `Some(Some)`
/// for an id.
pub fn requested_project(
    msg: &serde_json::Value,
) -> Result<Option<Option<String>>, MembershipError> {
    match msg.get("projectId") {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(Some(None)),
        Some(serde_json::Value::String(s)) => {
            let s = s.trim();
            Ok(Some((!s.is_empty()).then(|| s.to_string())))
        }
        Some(_) => Err(MembershipError::Malformed),
    }
}

/// The project a session ends up in, given what it has now and what the
/// client asked for. A session that exists keeps its project; asking for the
/// same one is fine (a client may echo the whole group back), asking for
/// anything else is [`MembershipError::Fixed`].
pub fn decide_membership(
    existing: Option<&GroupBinding>,
    requested: Option<Option<&str>>,
) -> Result<Option<String>, MembershipError> {
    match (existing, requested) {
        (None, None) | (None, Some(None)) => Ok(None),
        (None, Some(Some(id))) => Ok(Some(id.to_string())),
        (Some(e), None) => Ok(e.project_id.clone()),
        (Some(e), Some(asked)) if asked == e.project_id.as_deref() => Ok(e.project_id.clone()),
        (Some(_), Some(_)) => Err(MembershipError::Fixed),
    }
}

/// What a session in a project must look like.
pub fn check_member_shape(channel: &str, group_type: &str) -> Result<(), MembershipError> {
    if !channel.is_empty() {
        return Err(MembershipError::Channel);
    }
    if !matches!(group_type, "chat" | "code") {
        return Err(MembershipError::GroupType);
    }
    Ok(())
}

/// Whether a session can be created in `project` (`None` = no such project).
pub fn check_joinable(project: Option<&Project>) -> Result<(), MembershipError> {
    match project {
        None => Err(MembershipError::UnknownProject),
        Some(p) if p.is_archived() => Err(MembershipError::Archived),
        Some(_) => Ok(()),
    }
}

/// `SENCLAW_PROJECT_ROOT` as a child process sees it: unset and blank are the
/// same, "not in a project".
pub fn project_root_from_env() -> Option<String> {
    std::env::var(ENV_PROJECT_ROOT)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(jid: &str, folder: &str, project: Option<&str>) -> GroupBinding {
        GroupBinding {
            jid: jid.into(),
            folder: folder.into(),
            name: "n".into(),
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
            added_at: String::new(),
            project_id: project.map(String::from),
        }
    }

    #[test]
    fn outside_a_project_the_key_is_the_profile_folder_as_it_always_was() {
        assert_eq!(
            workspace_state_key(&binding("web:main:1", "main", None)),
            "main"
        );
        assert_eq!(
            workspace_state_file_name(&workspace_state_key(&binding("web:main:1", "main", None))),
            "workspace-state-main.json"
        );
    }

    #[test]
    fn inside_a_project_every_session_has_its_own_stable_key() {
        let a = binding("web:main:1", "main", Some("prj_00000001"));
        let b = binding("web:main:2", "main", Some("prj_00000001"));
        let ka = workspace_state_key(&a);
        assert_ne!(
            ka,
            workspace_state_key(&b),
            "same profile, same project, different sessions"
        );
        assert_eq!(ka, workspace_state_key(&a), "stable");
        assert!(
            ka.starts_with("main@") && ka.len() == "main@".len() + 12,
            "{ka}"
        );
        assert!(
            ka.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'@'),
            "file-name safe: {ka}"
        );
        // And never the profile's shared file.
        assert_ne!(ka, "main");
    }

    #[test]
    fn a_jid_with_path_characters_cannot_reach_another_folder() {
        let k = workspace_state_key(&binding(
            "web:../../etc/passwd",
            "main",
            Some("prj_00000001"),
        ));
        assert!(!k.contains('/') && !k.contains(".."), "{k}");
    }

    #[test]
    fn deleting_a_session_removes_its_own_file_and_never_the_profiles() {
        let home = tempfile::tempdir().unwrap();
        let profile_file = home.path().join("workspace-state-main.json");
        std::fs::write(&profile_file, "{}").unwrap();
        let inside = binding("web:main:1", "main", Some("prj_00000001"));
        let own = home
            .path()
            .join(workspace_state_file_name(&workspace_state_key(&inside)));
        std::fs::write(&own, "{}").unwrap();

        remove_session_state_file(home.path(), &inside.folder, &inside.jid);
        assert!(!own.exists());
        assert!(
            profile_file.exists(),
            "shared with the profile's other sessions"
        );
        // Removing twice is fine.
        remove_session_state_file(home.path(), &inside.folder, &inside.jid);
    }

    #[test]
    fn the_bank_is_named_after_the_project() {
        assert_eq!(project_bank("prj_0a1b2c3d"), "project:prj_0a1b2c3d");
    }
}

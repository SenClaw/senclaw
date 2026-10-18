//! Projects: a folder root plus what the person has written about it.
//!
//! A session created in a project works in the project's root, sees the
//! project's context in its system prompt on every turn, and keeps a
//! workspace-state file of its own (`session::workspace_state_key`) so two
//! sessions of one agent profile in two projects never share a working folder.
//!
//! This module depends on the database and the filesystem only — nothing from
//! `gateway` — so the REST layer, the agent pool and the MCP servers can all
//! use it without importing each other.
//!
//! - [`types`]: the row and its wire shape.
//! - [`store`]: SQL for the `projects` table (methods on [`crate::db::Db`]).
//! - [`root`]: which folders may be a root, and why not.
//! - [`context`]: `CONTEXT.md` on disk and the `<project>` prompt block.
//! - [`session`]: what a session needs from its project (state file, env).

pub mod context;
pub mod root;
pub mod session;
pub mod store;
pub mod types;

pub use context::{
    read_context, render_block, write_context, ContextFile, ContextWriteError, CONTEXT_FILE,
    CONTEXT_MAX_CHARS,
};
pub use root::{check_root, recheck_root, RootCheck, RootProblem, RootRules};
pub use session::{
    check_joinable, check_member_shape, decide_membership, project_bank, project_for_binding,
    project_root_from_env, remove_session_state_file, requested_project, session_state_key,
    workspace_state_file_name, workspace_state_key, MembershipError, ENV_PROJECT_BANK,
    ENV_PROJECT_ROOT,
};
pub use store::{ArchivedFilter, DetachedChat, NewProject, ProjectPatch, ProjectStoreError};
pub use types::{FactsWrite, Project};

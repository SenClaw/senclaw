//! Zen Patterns — named, reusable system prompts for one-shot text transforms.
//!
//! A pattern is a directory holding a `system.md`. That is the whole format;
//! it is deliberately the same one [Fabric](https://github.com/danielmiessler/fabric)
//! uses, so its ~250-pattern library imports without a converter.
//!
//! ```text
//! <patterns_dir>/
//!   sources.json                     # where patterns come from, in priority order
//!   user/<name>/system.md            # the built-in local source (always first)
//!   sources/<source-id>/…            # git clones + kit-installed sets
//!   strategies/<name>.json           # reasoning wrappers (cot, tot, reflexion…)
//! ```
//!
//! ## Why patterns are not skills
//!
//! The obvious implementation — one `SKILL.md` per pattern — breaks the agent.
//! [`crate::skills::scan`] loads every skill it finds into one registry, and
//! each contributes a name, a description and `triggers` to the pre-turn
//! matcher. Adding a few hundred entries drowns the real skills
//! (`web-research`, `agent-browser`) and floods the slash-command namespace.
//! Patterns therefore get their own registry and reach the agent through a
//! single skill plus a handful of MCP tools, no matter how many are installed.
//!
//! ## What a pattern is not
//!
//! No tools, no loop, no memory: text in, text out, one LLM call. That is the
//! property that makes the output stable enough to store or pipe. Anything
//! that needs to *do* something is a skill.

pub mod catalog;
pub mod registry;
pub mod render;
pub mod source;
pub mod store;
pub mod strategy;

pub use catalog::{install_starters, BundledPattern, CatalogEntry, STARTER_SOURCE_ID};
pub use registry::{PatternEntry, PatternRegistry};
pub use render::{render_pattern, RenderRequest, RenderedPattern};
pub use source::{sync_source, SourceSyncOutcome};
pub use store::{sanitize_name, PatternStore, StoreError};
pub use strategy::Strategy;

use serde::{Deserialize, Serialize};

/// The local, user-owned source. Always scanned first, so a pattern the user
/// writes shadows the same name coming from git — the rule Fabric states as
/// "your custom patterns won't be overwritten when you update".
pub const USER_SOURCE_ID: &str = "user";

/// Where a source's files come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// A plain directory under `<patterns_dir>`; the user or a kit writes into
    /// it directly.
    Local,
    /// A git clone kept under `<patterns_dir>/sources/<id>`, refreshed with
    /// [`source::sync_source`].
    Git,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Git => "git",
        }
    }
}

fn default_git_ref() -> String {
    "main".to_string()
}

fn default_true() -> bool {
    true
}

/// One place patterns are read from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatternSource {
    /// Slug, unique per daemon. Doubles as the directory name, so it is
    /// sanitized on the way in.
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub kind: SourceKind,
    /// Git remote. `None` for [`SourceKind::Local`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Branch or tag to check out.
    ///
    /// Defaults to `main` but a source that matters should pin a tag: a
    /// pattern lands in the **system prompt** position, so following a moving
    /// branch means an upstream edit silently rewrites instructions the agent
    /// obeys. See the module docs of [`source`].
    #[serde(default = "default_git_ref", alias = "ref")]
    pub git_ref: String,
    /// Sub-path inside the checkout that actually holds the pattern
    /// directories (`data/patterns` for Fabric). Empty = repo root.
    #[serde(default)]
    pub subdir: String,
    /// Sub-path holding `*.json` strategies, if the source ships any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategies_subdir: Option<String>,
    /// A disabled source stays on disk and keeps its files, but contributes
    /// nothing to the registry.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `kit:<id>` when a Zen Kit added it, so uninstalling that kit knows to
    /// take it back out. `None` = added by the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_by: Option<String>,
    /// RFC3339 UTC of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at: Option<String>,
    /// Why the last sync failed, cleared on the next success. Kept so the UI
    /// can show a stale source as stale instead of silently empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl PatternSource {
    /// The user's own local source, created on first use.
    pub fn user() -> Self {
        Self {
            id: USER_SOURCE_ID.to_string(),
            name: "My patterns".to_string(),
            kind: SourceKind::Local,
            url: None,
            git_ref: default_git_ref(),
            subdir: String::new(),
            strategies_subdir: None,
            enabled: true,
            installed_by: None,
            last_synced_at: None,
            last_error: None,
        }
    }

    /// A local source a kit owns. Kit patterns land here rather than in
    /// [`USER_SOURCE_ID`] so uninstalling the kit is a directory delete that
    /// cannot take a hand-written pattern with it.
    pub fn for_kit(kit_id: &str) -> Self {
        Self {
            id: format!("kit-{kit_id}"),
            name: format!("Kit: {kit_id}"),
            kind: SourceKind::Local,
            url: None,
            git_ref: default_git_ref(),
            subdir: String::new(),
            strategies_subdir: None,
            enabled: true,
            installed_by: Some(format!("kit:{kit_id}")),
            last_synced_at: None,
            last_error: None,
        }
    }

    /// True when this source's files may be edited or deleted through the API.
    ///
    /// A git checkout is not writable: the next sync would revert the edit and
    /// the user would have lost work with nothing to show for it. Editing a
    /// git-sourced pattern means saving a copy into [`USER_SOURCE_ID`], which
    /// then shadows it.
    pub fn writable(&self) -> bool {
        self.kind == SourceKind::Local
    }
}

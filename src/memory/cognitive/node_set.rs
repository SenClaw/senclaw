//! NodeSet — scope tagging for memory.
//!
//! Default policy: every node carries one or more NodeSets matching
//! `(scope_kind, scope_id)`. Three implicit scopes line up with edge tiers:
//!
//! | scope_kind | scope_id              | Typical tier home |
//! |------------|-----------------------|-------------------|
//! | `group`    | group jid             | L1 / L2           |
//! | `persona`  | persona slug          | L2                |
//! | `global`   | `""` (empty)          | L3                |
//! | `custom`   | free-form tag         | any               |
//!
//! This file only defines the types + helpers; the actual SQL is in
//! [`super::graph_store`].

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScopeKind {
    Group,
    Persona,
    Global,
    Custom,
}

impl ScopeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Persona => "persona",
            Self::Global => "global",
            Self::Custom => "custom",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "group" => Self::Group,
            "persona" => Self::Persona,
            "custom" => Self::Custom,
            _ => Self::Global,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeSet {
    pub scope_kind: ScopeKind,
    pub scope_id: String,
    pub tag: String,
}

impl NodeSet {
    pub fn group(jid: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            scope_kind: ScopeKind::Group,
            scope_id: jid.into(),
            tag: tag.into(),
        }
    }
    pub fn persona(slug: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            scope_kind: ScopeKind::Persona,
            scope_id: slug.into(),
            tag: tag.into(),
        }
    }
    pub fn global(tag: impl Into<String>) -> Self {
        Self {
            scope_kind: ScopeKind::Global,
            scope_id: String::new(),
            tag: tag.into(),
        }
    }
    pub fn custom(scope_id: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            scope_kind: ScopeKind::Custom,
            scope_id: scope_id.into(),
            tag: tag.into(),
        }
    }
    /// A named "knowledge space": an independent memory partition addressed
    /// by a single string id (e.g. `ai-office:nghien-cuu`). Spaces are
    /// `custom` scopes with the standard memory tag, so ingest/search can be
    /// isolated per space while unscoped queries still see everything.
    pub fn space(space_id: impl Into<String>) -> Self {
        Self::custom(space_id, "default_memory")
    }
}

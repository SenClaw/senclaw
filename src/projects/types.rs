//! The project row and its wire shape.

use serde::Serialize;

/// Where facts extracted from a project's chats are stored.
///
/// Kept with the project from the start so the column never needs a second
/// migration; nothing reads it until the fact store learns about projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FactsWrite {
    /// Into the project's own bank, shared by every chat of the project.
    Project,
    /// As if the chat were outside any project.
    Chat,
    /// Nowhere.
    Off,
}

impl FactsWrite {
    pub fn as_str(self) -> &'static str {
        match self {
            FactsWrite::Project => "project",
            FactsWrite::Chat => "chat",
            FactsWrite::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "project" => Some(FactsWrite::Project),
            "chat" => Some(FactsWrite::Chat),
            "off" => Some(FactsWrite::Off),
            _ => None,
        }
    }
}

/// One row of `projects`. Times are milliseconds since the epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    /// `prj_` + 8 hex digits. Stable: it names the project's folder under the
    /// data home and, later, its fact bank.
    pub id: String,
    pub name: String,
    /// Absolute and canonical (symlinks resolved), unique across projects.
    pub root: String,
    /// One or two lines for a list.
    pub description: String,
    pub facts_write: FactsWrite,
    pub inherit_common: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// Archived projects are hidden and take no new sessions; the sessions
    /// they already have keep working.
    pub archived_at: Option<i64>,
}

impl Project {
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

/// `prj_` followed by exactly 8 lowercase hex digits. Checked before an id is
/// used to build a path.
pub fn is_valid_id(id: &str) -> bool {
    id.len() == 12
        && id.starts_with("prj_")
        && id[4..]
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prj_and_eight_hex_digits() {
        assert!(is_valid_id("prj_0a1b2c3d"));
        for bad in [
            "",
            "prj_",
            "prj_0A1B2C3D",
            "prj_0a1b2c3",
            "prj_0a1b2c3de",
            "x_0a1b2c3d",
            "prj_../../ab",
        ] {
            assert!(!is_valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn facts_write_round_trips_its_three_spellings() {
        for w in [FactsWrite::Project, FactsWrite::Chat, FactsWrite::Off] {
            assert_eq!(FactsWrite::parse(w.as_str()), Some(w));
        }
        assert_eq!(FactsWrite::parse("all"), None);
    }
}

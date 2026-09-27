//! Resolving a pattern name across sources.
//!
//! Sources are scanned in ledger order and the **first hit wins**, with the
//! user source pinned first by [`super::store::PatternStore::sources`]. That
//! single rule is what lets someone fix a Fabric pattern they dislike — save a
//! copy under the same name and it shadows the checkout, surviving every
//! later `git pull`.

use serde::Serialize;

use super::store::{PatternFiles, PatternStore, StoreError};
use super::PatternSource;
use crate::util::text::truncate_on_char_boundary;

/// One row of the pattern list: enough to pick from, without loading bodies.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PatternEntry {
    pub name: String,
    /// Id of the source this name actually resolves to.
    pub source: String,
    /// First prose line of `system.md`.
    pub description: String,
    /// Other sources that also carry this name and are being shadowed. Shown
    /// in the UI so "I edited it and nothing changed" has a visible cause.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shadowed_in: Vec<String>,
    pub writable: bool,
}

/// Longest description kept per row. Fabric's opening paragraphs run long and
/// the list has hundreds of rows.
const DESC_MAX_BYTES: usize = 240;

/// Pull a one-line summary out of a pattern body.
///
/// Fabric's convention is `# IDENTITY and PURPOSE` followed by a blank line
/// and a prose paragraph, so the first non-heading, non-blank line is the
/// description. Sliced on a char boundary because pattern bodies are UTF-8 and
/// a byte slice through a Vietnamese character panics.
pub fn describe(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("---"))
        .unwrap_or("");
    let cut = truncate_on_char_boundary(line, DESC_MAX_BYTES);
    if cut.len() < line.len() {
        format!("{}…", cut.trim_end())
    } else {
        cut.to_string()
    }
}

/// Read-through view over every enabled source.
pub struct PatternRegistry<'a> {
    store: &'a PatternStore,
}

impl<'a> PatternRegistry<'a> {
    pub fn new(store: &'a PatternStore) -> Self {
        Self { store }
    }

    /// Enabled sources, in resolution order.
    fn enabled(&self) -> Vec<PatternSource> {
        self.store
            .sources()
            .into_iter()
            .filter(|s| s.enabled)
            .collect()
    }

    /// Which source a name resolves to, and its files.
    pub fn resolve(&self, name: &str) -> Result<(PatternSource, PatternFiles), StoreError> {
        for src in self.enabled() {
            if let Ok(files) = self.store.read(&src, name) {
                return Ok((src, files));
            }
        }
        Err(StoreError::NotFound(name.to_string()))
    }

    /// Just the names, deduplicated, without opening a single body.
    ///
    /// [`Self::list`] reads every `system.md` to build descriptions, which is
    /// 255 file reads for the Fabric library. The composer-directive pass runs
    /// on every message containing a `/token` and only needs to answer "is
    /// this a pattern name?", so it uses this instead.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for src in self.enabled() {
            for name in self.store.names_in(&src) {
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        out.sort();
        out
    }

    /// Every distinct pattern name, deduplicated by the shadowing rule.
    ///
    /// `query` filters on name and description, case-insensitively; an empty
    /// query returns everything.
    pub fn list(&self, query: &str, source_filter: Option<&str>) -> Vec<PatternEntry> {
        let q = query.trim().to_lowercase();
        let mut rows: Vec<PatternEntry> = Vec::new();

        for src in self.enabled() {
            for name in self.store.names_in(&src) {
                // Already resolved by an earlier (higher priority) source:
                // record the shadow and move on rather than emitting a second
                // row the caller would have to dedupe itself.
                if let Some(existing) = rows.iter_mut().find(|r| r.name == name) {
                    existing.shadowed_in.push(src.id.clone());
                    continue;
                }
                let Ok(files) = self.store.read(&src, &name) else {
                    continue;
                };
                rows.push(PatternEntry {
                    description: describe(&files.system),
                    name,
                    source: src.id.clone(),
                    shadowed_in: Vec::new(),
                    writable: src.writable(),
                });
            }
        }

        // Filtering happens after shadow resolution so a source filter shows
        // what that source *provides*, not what it would provide if nothing
        // outranked it.
        rows.retain(|r| match source_filter {
            Some(f) if !f.is_empty() => r.source == f,
            _ => true,
        });
        if !q.is_empty() {
            rows.retain(|r| {
                r.name.to_lowercase().contains(&q) || r.description.to_lowercase().contains(&q)
            });
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{SourceKind, USER_SOURCE_ID};

    fn store_in(dir: &std::path::Path) -> PatternStore {
        PatternStore::new(dir)
    }

    #[test]
    fn describe_takes_the_first_prose_line_not_the_heading() {
        let body = "# IDENTITY and PURPOSE\n\nYou are an expert content summarizer.\n\n# STEPS";
        assert_eq!(describe(body), "You are an expert content summarizer.");
    }

    #[test]
    fn describe_truncates_on_a_char_boundary() {
        let body = format!("# H\n\n{}", "á".repeat(400));
        let d = describe(&body);
        assert!(d.ends_with('…'));
        assert!(d.len() <= DESC_MAX_BYTES + 4);
    }

    #[test]
    fn user_source_shadows_a_git_pattern_of_the_same_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());

        let fabric = PatternSource {
            id: "fabric".into(),
            kind: SourceKind::Local, // Local so the test can write into it.
            ..PatternSource::for_kit("fabric")
        };
        store
            .save_sources(&[PatternSource::user(), fabric.clone()])
            .unwrap();

        store
            .write(&fabric, "summarize", "# H\n\nUpstream version.", None, false)
            .unwrap();
        store
            .write(
                &PatternSource::user(),
                "summarize",
                "# H\n\nMy version.",
                None,
                false,
            )
            .unwrap();

        let reg = PatternRegistry::new(&store);
        let (src, files) = reg.resolve("summarize").unwrap();
        assert_eq!(src.id, USER_SOURCE_ID);
        assert!(files.system.contains("My version."));

        let rows = reg.list("", None);
        assert_eq!(rows.len(), 1, "one row per name, not one per source");
        assert_eq!(rows[0].source, USER_SOURCE_ID);
        assert_eq!(rows[0].shadowed_in, vec!["fabric".to_string()]);
    }

    #[test]
    fn disabled_sources_contribute_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let off = PatternSource {
            enabled: false,
            ..PatternSource::for_kit("off")
        };
        // Written while conceptually enabled — disabling must hide it, not
        // delete it.
        let on = PatternSource {
            enabled: true,
            ..off.clone()
        };
        store.write(&on, "hidden", "# H\n\nx", None, false).unwrap();
        store.save_sources(&[PatternSource::user(), off]).unwrap();

        let reg = PatternRegistry::new(&store);
        assert!(reg.list("", None).is_empty());
        assert!(reg.resolve("hidden").is_err());
    }

    #[test]
    fn list_filters_by_query_and_source() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let user = PatternSource::user();
        store
            .write(&user, "summarize", "# H\n\nSummarise content.", None, false)
            .unwrap();
        store
            .write(&user, "analyze_logs", "# H\n\nRead server logs.", None, false)
            .unwrap();

        let reg = PatternRegistry::new(&store);
        assert_eq!(reg.list("log", None).len(), 1);
        assert_eq!(reg.list("content", None).len(), 1, "matches description too");
        assert_eq!(reg.list("", Some("user")).len(), 2);
        assert!(reg.list("", Some("nope")).is_empty());
    }
}

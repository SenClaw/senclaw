//! On-disk layout for patterns: where files live, and the source ledger.
//!
//! Everything a caller does to the filesystem goes through [`PatternStore`],
//! which owns `<patterns_dir>` and refuses to build a path outside it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{PatternSource, SourceKind, USER_SOURCE_ID};

/// What a store operation can refuse to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The name could not be reduced to a safe slug.
    BadName(String),
    /// No such pattern in the given source.
    NotFound(String),
    /// No such source id.
    NoSource(String),
    /// Tried to write into a git checkout.
    ReadOnly(String),
    /// The name is already taken and `overwrite` was not set.
    Exists(String),
    Io(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName(n) => write!(
                f,
                "\"{n}\" is not a usable pattern name (letters, digits, - and _ only)"
            ),
            Self::NotFound(n) => write!(f, "pattern \"{n}\" not found"),
            Self::NoSource(id) => write!(f, "pattern source \"{id}\" not found"),
            Self::ReadOnly(id) => write!(
                f,
                "source \"{id}\" is read-only — save your edit to the \"{USER_SOURCE_ID}\" source instead, it takes priority"
            ),
            Self::Exists(n) => write!(f, "pattern \"{n}\" already exists here"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// Reduce arbitrary text to a directory-safe pattern id.
///
/// Names arrive from three places that are all outside our control — a git
/// repo's directory listing, a kit manifest, and a UI text field — so this is
/// the single choke point that keeps `../../.ssh` and an absolute path out of
/// [`PatternStore::pattern_dir`]. Anything outside `[a-z0-9_-]` becomes `_`,
/// and a name that reduces to nothing is rejected rather than silently turned
/// into a directory called `_`.
///
/// **Diacritics fold to their base letter first**, sharing the table
/// [`crate::security::replication::fold`] — the same rule `senclaw create`
/// uses for ids. Mapping them straight to `_` like any other non-ASCII
/// character turns "Tóm Tắt Thử" into `t_m_t_t_th`, which is neither typeable
/// nor recognisable; folding gives `tom_tat_thu`.
pub fn sanitize_name(raw: &str) -> Result<String> {
    let slug: String = crate::security::replication::fold(raw.trim())
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            // Whatever the fold table does not cover (CJK, emoji, punctuation)
            // still folds to `_`: a directory name the CLI cannot type is
            // worse, and the display name lives in the pattern body anyway.
            _ => '_',
        })
        .collect();

    // Collapse runs of `_` so "a / b" does not become "a___b", then strip the
    // separators from the ends where they read as accidental.
    let mut collapsed = String::with_capacity(slug.len());
    let mut prev_us = false;
    for c in slug.chars() {
        if c == '_' {
            if !prev_us {
                collapsed.push(c);
            }
            prev_us = true;
        } else {
            collapsed.push(c);
            prev_us = false;
        }
    }
    let trimmed = collapsed.trim_matches('_').trim_matches('-');

    if trimmed.is_empty() {
        return Err(StoreError::BadName(raw.to_string()));
    }
    Ok(trimmed.to_string())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SourcesFile {
    #[serde(default)]
    sources: Vec<PatternSource>,
}

/// A pattern's files, as read off disk.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PatternFiles {
    pub name: String,
    pub source: String,
    /// `system.md` — the system prompt. Always present.
    pub system: String,
    /// `user.md` — an optional user-message template some Fabric patterns
    /// ship. When absent the caller's input becomes the user message directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub path: String,
    /// False for a git checkout — see [`PatternSource::writable`].
    pub writable: bool,
}

/// Owns `<patterns_dir>` and every path built under it.
pub struct PatternStore {
    root: PathBuf,
}

impl PatternStore {
    pub fn new(patterns_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: patterns_dir.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn sources_path(&self) -> PathBuf {
        self.root.join("sources.json")
    }

    pub fn strategies_dir(&self) -> PathBuf {
        self.root.join("strategies")
    }

    /// Directory holding a source's pattern folders.
    ///
    /// A local source is a plain folder named after its id; a git source lives
    /// under `sources/` and points at `subdir` inside the checkout.
    pub fn source_dir(&self, src: &PatternSource) -> PathBuf {
        let base = match src.kind {
            SourceKind::Local => self.root.join(&src.id),
            SourceKind::Git => self.checkout_dir(&src.id),
        };
        if src.subdir.is_empty() {
            base
        } else {
            // Joined component by component, dropping `..`, so a hand-edited
            // sources.json cannot point the scanner outside the checkout.
            src.subdir
                .split(['/', '\\'])
                .filter(|s| !s.is_empty() && *s != "." && *s != "..")
                .fold(base, |acc, seg| acc.join(seg))
        }
    }

    /// Where a git source's working tree is cloned.
    pub fn checkout_dir(&self, source_id: &str) -> PathBuf {
        self.root.join("sources").join(source_id)
    }

    /// Absolute directory of one pattern inside one source.
    pub fn pattern_dir(&self, src: &PatternSource, name: &str) -> Result<PathBuf> {
        let safe = sanitize_name(name)?;
        Ok(self.source_dir(src).join(safe))
    }

    // ===== sources.json =====

    /// Read the source ledger, seeding it with the user source on first run.
    ///
    /// A corrupt file reads as "just the user source" rather than failing every
    /// call: losing the ledger costs the git sources, while erroring out would
    /// take the whole patterns API down with it — the same trade
    /// [`crate::kits::receipt`] makes.
    pub fn sources(&self) -> Vec<PatternSource> {
        let mut list = fs::read_to_string(self.sources_path())
            .ok()
            .and_then(|raw| serde_json::from_str::<SourcesFile>(&raw).ok())
            .map(|f| f.sources)
            .unwrap_or_default();

        match list.iter().position(|s| s.id == USER_SOURCE_ID) {
            // The user source is the shadowing rule; it only works if it is
            // scanned first, so its position is ours to decide, not the file's.
            Some(pos) => {
                let user = list.remove(pos);
                list.insert(0, user);
            }
            None => list.insert(0, PatternSource::user()),
        }
        list
    }

    pub fn source(&self, id: &str) -> Result<PatternSource> {
        self.sources()
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| StoreError::NoSource(id.to_string()))
    }

    pub fn save_sources(&self, sources: &[PatternSource]) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let body = serde_json::to_string_pretty(&SourcesFile {
            sources: sources.to_vec(),
        })
        .map_err(|e| StoreError::Io(e.to_string()))?;
        write_atomic(&self.sources_path(), body.as_bytes())
    }

    /// Insert or replace one source, keeping the user source pinned first.
    pub fn upsert_source(&self, src: PatternSource) -> Result<()> {
        let mut all = self.sources();
        match all.iter().position(|s| s.id == src.id) {
            Some(i) => all[i] = src,
            None => all.push(src),
        }
        self.save_sources(&all)
    }

    /// Drop a source from the ledger and delete its files.
    ///
    /// The user source is never removable — it is where "save a copy" writes,
    /// and a daemon without it has no way to accept a new pattern.
    pub fn remove_source(&self, id: &str) -> Result<()> {
        if id == USER_SOURCE_ID {
            return Err(StoreError::ReadOnly(id.to_string()));
        }
        let src = self.source(id)?;
        let files = match src.kind {
            SourceKind::Local => self.root.join(&src.id),
            SourceKind::Git => self.checkout_dir(&src.id),
        };
        // A failed delete must not leave the source in the ledger pointing at
        // files nobody will look at again; report it, but still de-register.
        let rm = fs::remove_dir_all(&files);
        let remaining: Vec<PatternSource> =
            self.sources().into_iter().filter(|s| s.id != id).collect();
        self.save_sources(&remaining)?;
        match rm {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    // ===== pattern files =====

    /// Read one pattern out of one source.
    pub fn read(&self, src: &PatternSource, name: &str) -> Result<PatternFiles> {
        let dir = self.pattern_dir(src, name)?;
        let system =
            fs::read_to_string(dir.join("system.md")).map_err(|_| StoreError::NotFound(name.to_string()))?;
        let user = fs::read_to_string(dir.join("user.md")).ok();
        Ok(PatternFiles {
            name: sanitize_name(name)?,
            source: src.id.clone(),
            system,
            user,
            path: dir.display().to_string(),
            writable: src.writable(),
        })
    }

    /// Create or replace a pattern in a writable source.
    pub fn write(
        &self,
        src: &PatternSource,
        name: &str,
        system: &str,
        user: Option<&str>,
        overwrite: bool,
    ) -> Result<PatternFiles> {
        if !src.writable() {
            return Err(StoreError::ReadOnly(src.id.clone()));
        }
        let safe = sanitize_name(name)?;
        let dir = self.pattern_dir(src, &safe)?;
        if dir.join("system.md").exists() && !overwrite {
            return Err(StoreError::Exists(safe));
        }
        fs::create_dir_all(&dir)?;
        write_atomic(&dir.join("system.md"), system.as_bytes())?;
        match user {
            Some(body) if !body.trim().is_empty() => {
                write_atomic(&dir.join("user.md"), body.as_bytes())?;
            }
            // An explicit empty `user` removes a template that used to be
            // there; leaving the old file would keep applying it invisibly.
            Some(_) => {
                let _ = fs::remove_file(dir.join("user.md"));
            }
            None => {}
        }
        self.read(src, &safe)
    }

    pub fn delete(&self, src: &PatternSource, name: &str) -> Result<()> {
        if !src.writable() {
            return Err(StoreError::ReadOnly(src.id.clone()));
        }
        let dir = self.pattern_dir(src, name)?;
        if !dir.exists() {
            return Err(StoreError::NotFound(name.to_string()));
        }
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// Pattern names present in one source, sorted.
    ///
    /// A directory without a `system.md` is not a pattern — Fabric's tree has
    /// stray folders and a `README.md`, and treating those as empty patterns
    /// would put unusable names in the picker.
    pub fn names_in(&self, src: &PatternSource) -> Vec<String> {
        let Ok(rd) = fs::read_dir(self.source_dir(src)) else {
            return Vec::new();
        };
        let mut out: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir() && e.path().join("system.md").is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| !n.starts_with('.'))
            .collect();
        out.sort();
        out
    }

    /// Copy every pattern from a directory tree into a source, returning the
    /// names written. Used by the kit installer and the folder/zip importer.
    pub fn import_tree(
        &self,
        src: &PatternSource,
        tree: &Path,
        overwrite: bool,
    ) -> Result<Vec<String>> {
        let Ok(rd) = fs::read_dir(tree) else {
            return Ok(Vec::new());
        };
        let mut written = Vec::new();
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Ok(system) = fs::read_to_string(path.join("system.md")) else {
                continue;
            };
            let Some(raw_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let user = fs::read_to_string(path.join("user.md")).ok();
            // One bad name must not abort an import of 250 good ones.
            match self.write(src, raw_name, &system, user.as_deref(), overwrite) {
                Ok(files) => written.push(files.name),
                Err(StoreError::Exists(_)) => {}
                Err(e) => tracing::warn!("[patterns] skipped \"{raw_name}\": {e}"),
            }
        }
        written.sort();
        Ok(written)
    }

    /// Copy `*.json` strategies out of a synced source into the shared
    /// strategies directory, skipping names that already exist.
    ///
    /// Strategies are global rather than per-source because they are two-line
    /// reasoning wrappers with conventional names (`cot`, `tot`); one `cot` is
    /// the useful outcome, not one per repo.
    pub fn import_strategies(&self, from: &Path) -> Result<Vec<String>> {
        let Ok(rd) = fs::read_dir(from) else {
            return Ok(Vec::new());
        };
        let dest = self.strategies_dir();
        fs::create_dir_all(&dest)?;
        let mut written = Vec::new();
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(safe) = sanitize_name(stem) else {
                continue;
            };
            let target = dest.join(format!("{safe}.json"));
            if target.exists() {
                continue;
            }
            // Only accept files that parse as a strategy; a stray JSON in the
            // folder would otherwise show up in the picker and fail at render.
            if let Ok(body) = fs::read(&path) {
                if serde_json::from_slice::<super::strategy::Strategy>(&body).is_ok()
                    && write_atomic(&target, &body).is_ok()
                {
                    written.push(safe);
                }
            }
        }
        written.sort();
        Ok(written)
    }

    /// Count of usable patterns per source id.
    pub fn counts(&self) -> BTreeMap<String, usize> {
        self.sources()
            .iter()
            .map(|s| (s.id.clone(), self.names_in(s).len()))
            .collect()
    }
}

/// Write via a temp file + rename so a crash mid-write cannot leave a
/// half-written `system.md` that still parses as a pattern.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PatternStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = PatternStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn sanitize_rejects_traversal_and_keeps_fabric_names() {
        assert_eq!(sanitize_name("extract_wisdom").unwrap(), "extract_wisdom");
        assert_eq!(sanitize_name("Analyze Claims").unwrap(), "analyze_claims");
        // Vietnamese folds to its base letters instead of collapsing to
        // punctuation soup — a live daemon produced `t_m_t_t_th` before this.
        assert_eq!(sanitize_name("Tóm Tắt Thử").unwrap(), "tom_tat_thu");
        assert_eq!(sanitize_name("Phân tích log").unwrap(), "phan_tich_log");
        assert_eq!(sanitize_name("Quản lý Kho").unwrap(), "quan_ly_kho");
        // The whole point: a name can never reach out of the patterns dir.
        assert_eq!(sanitize_name("../../.ssh").unwrap(), "ssh");
        assert_eq!(sanitize_name("/etc/passwd").unwrap(), "etc_passwd");
        assert!(sanitize_name("   ").is_err());
        assert!(sanitize_name("../..").is_err());
    }

    #[test]
    fn user_source_is_seeded_and_always_first() {
        let (_d, s) = store();
        assert_eq!(s.sources()[0].id, USER_SOURCE_ID);

        // Even if the file lists it last, the shadowing rule needs it first.
        s.save_sources(&[PatternSource::for_kit("fabric"), PatternSource::user()])
            .unwrap();
        assert_eq!(s.sources()[0].id, USER_SOURCE_ID);
        assert_eq!(s.sources().len(), 2);
    }

    #[test]
    fn write_read_delete_round_trip() {
        let (_d, s) = store();
        let user = PatternSource::user();
        let files = s
            .write(&user, "My Pattern", "# IDENTITY\nhello", None, false)
            .unwrap();
        assert_eq!(files.name, "my_pattern");
        assert!(files.writable);

        assert_eq!(s.names_in(&user), vec!["my_pattern"]);
        assert_eq!(
            s.read(&user, "my_pattern").unwrap().system,
            "# IDENTITY\nhello"
        );

        // Same name twice without overwrite is refused, not silently merged.
        assert!(matches!(
            s.write(&user, "my_pattern", "x", None, false),
            Err(StoreError::Exists(_))
        ));
        assert!(s.write(&user, "my_pattern", "x", None, true).is_ok());

        s.delete(&user, "my_pattern").unwrap();
        assert!(s.names_in(&user).is_empty());
    }

    #[test]
    fn git_source_is_read_only_and_stays_inside_its_checkout() {
        let (_d, s) = store();
        let git = PatternSource {
            id: "fabric".into(),
            kind: SourceKind::Git,
            url: Some("https://example.invalid/fabric".into()),
            subdir: "data/patterns".into(),
            ..PatternSource::for_kit("fabric")
        };
        assert!(!git.writable());
        assert!(matches!(
            s.write(&git, "x", "y", None, false),
            Err(StoreError::ReadOnly(_))
        ));
        assert!(s.source_dir(&git).starts_with(s.checkout_dir("fabric")));
    }

    #[test]
    fn subdir_cannot_escape_the_checkout() {
        let (_d, s) = store();
        let hostile = PatternSource {
            subdir: "../../../etc".into(),
            kind: SourceKind::Git,
            ..PatternSource::for_kit("evil")
        };
        assert!(s.source_dir(&hostile).starts_with(s.root()));
    }

    #[test]
    fn names_in_ignores_folders_without_system_md() {
        let (_d, s) = store();
        let user = PatternSource::user();
        s.write(&user, "good", "body", None, false).unwrap();
        fs::create_dir_all(s.root().join("user").join("empty")).unwrap();
        fs::write(s.root().join("user").join("README.md"), "x").unwrap();
        assert_eq!(s.names_in(&user), vec!["good"]);
    }

    #[test]
    fn import_tree_copies_patterns_and_skips_junk() {
        let (_d, s) = store();
        let tree = tempfile::tempdir().unwrap();
        for name in ["summarize", "extract_wisdom"] {
            let d = tree.path().join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("system.md"), format!("# {name}")).unwrap();
        }
        fs::create_dir_all(tree.path().join("not_a_pattern")).unwrap();
        fs::write(tree.path().join("README.md"), "hi").unwrap();

        let kit = PatternSource::for_kit("fabric");
        let written = s.import_tree(&kit, tree.path(), false).unwrap();
        assert_eq!(written, vec!["extract_wisdom", "summarize"]);
    }

    #[test]
    fn remove_source_refuses_the_user_source() {
        let (_d, s) = store();
        assert!(matches!(
            s.remove_source(USER_SOURCE_ID),
            Err(StoreError::ReadOnly(_))
        ));
    }
}

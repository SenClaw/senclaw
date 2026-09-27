//! Project documentation: markdown that lives **in the project being worked
//! on**, not in the user's global wiki.
//!
//! Three things already wrote markdown before this module, and none of them
//! could answer "what did you change, and where do I read about it?":
//!
//! - **Plan mode** writes one plan file, but it is intent recorded *before*
//!   the work, and it is gone the moment plan mode is off.
//! - **The wiki** (`~/senclaw/wiki`) is the user's own knowledge base, shared
//!   across every project. Documentation about a repository's code belongs
//!   with that repository: it has to survive a clone onto another machine.
//! - **`checkpoint_explain`** already narrates a diff, but only into chat,
//!   where it scrolls away.
//!
//! So this is the missing piece: a place inside the working directory, with a
//! convention, an index, and a hard boundary around where writes may land.
//!
//! The boundary is the whole reason this is not just `Write`. `Write` has no
//! convention (every doc lands somewhere different), no index (a doc nobody
//! can find is a doc nobody wrote), and no confinement — an agent that
//! mistakes a path writes into `$HOME`.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Markers around the generated block in `docs/README.md`. Anything outside
/// them is the user's own text and is preserved verbatim across rebuilds.
const INDEX_BEGIN: &str = "<!-- senclaw:docs-index:begin -->";
const INDEX_END: &str = "<!-- senclaw:docs-index:end -->";

/// A documentation file as the index and the `doc_list` tool see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocEntry {
    /// Path relative to the docs root, e.g. `changes/2026-09-12-steps.md`.
    pub path: String,
    /// First heading, else the file name.
    pub title: String,
    /// First prose line after the heading, trimmed and one line.
    pub summary: String,
}

/// What a write did. `created` distinguishes a new document from an updated
/// one so the answer to the user can say which it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    pub absolute_path: PathBuf,
    pub relative_path: String,
    pub created: bool,
}

/// The language existing documentation is written in.
///
/// Not a setting: a project whose `docs/` is Vietnamese should keep getting
/// Vietnamese, and one with no docs yet has no opinion to honour. Reported to
/// the agent so it writes in the language the project already uses rather
/// than the language of whichever question happened to trigger the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocsLanguage {
    Vietnamese,
    English,
    /// No documentation yet, so nothing to match.
    Unknown,
}

impl DocsLanguage {
    pub fn as_str(self) -> &'static str {
        match self {
            DocsLanguage::Vietnamese => "vi",
            DocsLanguage::English => "en",
            DocsLanguage::Unknown => "unknown",
        }
    }
}

/// The `docs/` directory of one project.
#[derive(Debug, Clone)]
pub struct DocsStore {
    root: PathBuf,
}

impl DocsStore {
    /// `working_dir` is the chat's current workspace; the store is its
    /// `docs/` subdirectory.
    pub fn for_working_dir(working_dir: impl AsRef<Path>) -> Self {
        Self {
            root: working_dir.as_ref().join("docs"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// True when the project already keeps documentation here.
    ///
    /// Callers use this to avoid creating a directory structure inside
    /// somebody's project without being asked.
    pub fn exists(&self) -> bool {
        self.root.is_dir()
    }

    /// Resolve a caller-supplied relative path to an absolute one inside the
    /// store, or fail.
    ///
    /// Rejected: absolute paths, `..` in any position, a path that after
    /// symlink resolution lands outside the root. The lexical check alone is
    /// not enough — `docs/link` pointing at `/etc` is a perfectly ordinary
    /// path with no `..` in it.
    pub fn resolve(&self, relative: &str) -> Result<PathBuf> {
        let rel = relative.trim();
        if rel.is_empty() {
            bail!("empty document path");
        }
        let candidate = Path::new(rel);
        if candidate.is_absolute() {
            bail!("document path must be relative to the project's docs/ directory: {rel}");
        }
        let mut clean = PathBuf::new();
        for part in candidate.components() {
            match part {
                Component::Normal(p) => clean.push(p),
                Component::CurDir => {}
                Component::ParentDir => {
                    bail!("document path must not climb out of docs/ with `..`: {rel}")
                }
                Component::RootDir | Component::Prefix(_) => {
                    bail!("document path must be relative: {rel}")
                }
            }
        }
        if clean.as_os_str().is_empty() {
            bail!("empty document path");
        }
        if clean.extension().and_then(|e| e.to_str()) != Some("md") {
            bail!("a document must be a .md file: {rel}");
        }
        let full = self.root.join(&clean);

        // Symlink check. Both sides go through the same resolution, because
        // canonicalizing only one of them compares two spellings of the same
        // place: on macOS a temporary directory is `/var/...` on one side and
        // `/private/var/...` on the other, and every write looks like an
        // escape.
        if !resolve_as_far_as_it_exists(&full).starts_with(resolve_as_far_as_it_exists(&self.root)) {
            bail!("document path resolves outside docs/: {rel}");
        }
        Ok(full)
    }

    /// Write one document. Refuses to clobber an existing file unless
    /// `overwrite` is set: a hand-written document is not this tool's to
    /// replace on a guess.
    pub fn write(&self, relative: &str, body: &str, overwrite: bool) -> Result<WriteOutcome> {
        let full = self.resolve(relative)?;
        let existed = full.exists();
        if existed && !overwrite {
            bail!(
                "{} already exists; pass overwrite to replace it, or write to a new path",
                full.display()
            );
        }
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let mut text = body.trim_end().to_string();
        text.push('\n');
        fs::write(&full, text).with_context(|| format!("write {}", full.display()))?;
        Ok(WriteOutcome {
            relative_path: rel_display(&self.root, &full),
            absolute_path: full,
            created: !existed,
        })
    }

    pub fn read(&self, relative: &str) -> Result<String> {
        let full = self.resolve(relative)?;
        fs::read_to_string(&full).with_context(|| format!("read {}", full.display()))
    }

    /// Every `.md` under the root, sorted by path, with its title and summary.
    /// The index file itself is left out — it is generated *from* this list.
    pub fn list(&self) -> Vec<DocEntry> {
        let mut out = Vec::new();
        collect_markdown(&self.root, &self.root, &mut out);
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Which language this project's existing docs are in.
    pub fn language(&self) -> DocsLanguage {
        let entries = self.list();
        if entries.is_empty() {
            return DocsLanguage::Unknown;
        }
        // Read titles and summaries rather than whole files: enough signal,
        // and bounded work on a repo with hundreds of documents.
        let sample: String = entries
            .iter()
            .map(|e| format!("{} {}", e.title, e.summary))
            .collect::<Vec<_>>()
            .join(" ");
        if has_vietnamese_diacritics(&sample) {
            DocsLanguage::Vietnamese
        } else {
            DocsLanguage::English
        }
    }

    /// Rewrite the generated block of `docs/README.md`.
    ///
    /// Only the block between the markers is replaced; a README with a hand-
    /// written introduction keeps it. A README with no markers gets the block
    /// appended once, so adopting this on an existing project is not
    /// destructive.
    pub fn rebuild_index(&self) -> Result<Option<PathBuf>> {
        if !self.root.is_dir() {
            return Ok(None);
        }
        let entries = self.list();
        let mut block = String::from(INDEX_BEGIN);
        block.push_str("\n\n| Tài liệu | Mô tả |\n|---|---|\n");
        for e in &entries {
            block.push_str(&format!(
                "| [{}]({}) | {} |\n",
                escape_cell(&e.title),
                e.path,
                escape_cell(&e.summary)
            ));
        }
        if entries.is_empty() {
            block.push_str("| _(chưa có tài liệu nào)_ | |\n");
        }
        block.push('\n');
        block.push_str(INDEX_END);

        let index_path = self.root.join("README.md");
        let existing = fs::read_to_string(&index_path).unwrap_or_default();
        let updated = splice_block(&existing, &block);
        fs::write(&index_path, updated)
            .with_context(|| format!("write {}", index_path.display()))?;
        Ok(Some(index_path))
    }
}

/// Replace the marked block in `existing`, or append it when absent.
fn splice_block(existing: &str, block: &str) -> String {
    match (existing.find(INDEX_BEGIN), existing.find(INDEX_END)) {
        (Some(start), Some(end)) if end > start => {
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..start]);
            out.push_str(block);
            out.push_str(&existing[end + INDEX_END.len()..]);
            out
        }
        _ => {
            let mut out = String::new();
            if existing.trim().is_empty() {
                out.push_str("# Tài liệu dự án\n\n");
            } else {
                out.push_str(existing.trim_end());
                out.push_str("\n\n");
            }
            out.push_str(block);
            out.push('\n');
            out
        }
    }
}

/// A kebab-case file-name fragment from a human title.
///
/// Diacritics fold through the same table the replication module uses, so
/// "Chuyển tool thành bước" becomes `chuyen-tool-thanh-buoc` rather than a
/// string of percent escapes or a name the shell fights with.
pub fn slugify(title: &str) -> String {
    let folded = crate::security::replication::fold(title);
    let mut out = String::new();
    let mut last_dash = true; // leading dashes are never wanted
    for ch in folded.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("ghi-chu");
    }
    if out.len() > 60 {
        out.truncate(60);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

/// Canonicalize the deepest part of `p` that exists, then re-append the rest.
///
/// `Path::canonicalize` fails outright on a path whose last components are not
/// created yet, which is the normal case for a document about to be written.
/// This resolves the symlinks that *can* be resolved and leaves the remainder
/// lexical.
fn resolve_as_far_as_it_exists(p: &Path) -> PathBuf {
    let mut probe = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if probe.exists() {
            let mut base = probe.canonicalize().unwrap_or(probe);
            for part in tail.iter().rev() {
                base.push(part);
            }
            return base;
        }
        match probe.file_name() {
            Some(name) => tail.push(name.to_os_string()),
            None => return p.to_path_buf(),
        }
        if !probe.pop() {
            return p.to_path_buf();
        }
    }
}

fn escape_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn rel_display(root: &Path, full: &Path) -> String {
    full.strip_prefix(root)
        .unwrap_or(full)
        .to_string_lossy()
        .replace('\\', "/")
}

fn collect_markdown(root: &Path, dir: &Path, out: &mut Vec<DocEntry>) {
    let Ok(read) = fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_markdown(root, &path, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let rel = rel_display(root, &path);
        // The index is generated from this list; listing it inside itself is
        // a row that only ever points back at the reader.
        if rel.eq_ignore_ascii_case("README.md") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_default();
        let (title, summary) = title_and_summary(&text, &rel);
        out.push(DocEntry {
            path: rel,
            title,
            summary,
        });
    }
}

/// First `# heading` and the first prose line after it.
///
/// Frontmatter, blank lines, and markdown emphasis markers are skipped so the
/// index row reads as a sentence rather than as source.
fn title_and_summary(text: &str, fallback: &str) -> (String, String) {
    let mut lines = text.lines().peekable();
    // Skip YAML frontmatter when present.
    if lines.peek().map(|l| l.trim()) == Some("---") {
        lines.next();
        for line in lines.by_ref() {
            if line.trim() == "---" {
                break;
            }
        }
    }
    let mut title = String::new();
    let mut summary = String::new();
    for line in lines {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if title.is_empty() {
            title = t.trim_start_matches('#').trim().to_string();
            continue;
        }
        if t.starts_with('#') {
            // A second heading with no prose between: nothing to summarise.
            break;
        }
        summary = t.trim_matches('*').trim().to_string();
        break;
    }
    if title.is_empty() {
        title = fallback.to_string();
    }
    if summary.len() > 160 {
        summary = format!(
            "{}…",
            crate::util::text::truncate_on_char_boundary(&summary, 160)
        );
    }
    (title, summary)
}

/// Vietnamese-specific letters. Deliberately not a general language detector:
/// the only question is whether this project's docs are Vietnamese, and these
/// characters appear in no English text.
fn has_vietnamese_diacritics(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c,
            'ă' | 'â' | 'đ' | 'ê' | 'ô' | 'ơ' | 'ư'
            | 'Ă' | 'Â' | 'Đ' | 'Ê' | 'Ô' | 'Ơ' | 'Ư'
            | 'ạ' | 'ả' | 'ấ' | 'ầ' | 'ệ' | 'ọ' | 'ộ' | 'ứ' | 'ừ' | 'ữ' | 'ỳ' | 'ỹ'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, DocsStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DocsStore::for_working_dir(dir.path());
        (dir, store)
    }

    #[test]
    fn a_write_lands_inside_the_projects_docs_directory() {
        let (dir, store) = store();
        let out = store
            .write("changes/2026-09-12-steps.md", "# Steps\n\nWhat changed.", false)
            .expect("write");
        assert!(out.created);
        assert_eq!(out.relative_path, "changes/2026-09-12-steps.md");
        assert!(out.absolute_path.starts_with(dir.path()));
        assert_eq!(
            fs::read_to_string(&out.absolute_path).unwrap(),
            "# Steps\n\nWhat changed.\n"
        );
    }

    #[test]
    fn a_path_that_climbs_out_is_refused() {
        let (_dir, store) = store();
        for bad in [
            "../escape.md",
            "changes/../../escape.md",
            "/etc/passwd.md",
            "",
            "notes.txt",
        ] {
            assert!(
                store.resolve(bad).is_err(),
                "expected {bad:?} to be refused"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_out_of_docs_is_refused() {
        // No `..` anywhere in this path — the lexical check passes it, and only
        // resolving the link catches it.
        let (dir, store) = store();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(store.root()).unwrap();
        std::os::unix::fs::symlink(&outside, store.root().join("link")).unwrap();
        assert!(store.resolve("link/leak.md").is_err());
    }

    #[test]
    fn an_existing_document_is_not_clobbered_without_being_asked() {
        let (_dir, store) = store();
        store.write("a.md", "# A\n\nfirst", false).unwrap();
        let err = store.write("a.md", "# A\n\nsecond", false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // The original survived the refusal.
        assert!(store.read("a.md").unwrap().contains("first"));
        // And the explicit overwrite works.
        store.write("a.md", "# A\n\nsecond", true).unwrap();
        assert!(store.read("a.md").unwrap().contains("second"));
    }

    #[test]
    fn the_listing_carries_a_title_and_a_summary() {
        let (_dir, store) = store();
        store
            .write(
                "one.md",
                "---\nname: x\n---\n\n# Chuyển tool thành bước\n\nMỗi bước có tiêu đề riêng.\n",
                false,
            )
            .unwrap();
        store.write("two.md", "no heading at all\n", false).unwrap();
        let list = store.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].title, "Chuyển tool thành bước");
        assert_eq!(list[0].summary, "Mỗi bước có tiêu đề riêng.");
        // A file with no heading still lists, under its own name.
        assert_eq!(list[1].title, "no heading at all");
    }

    #[test]
    fn the_index_keeps_whatever_a_person_wrote_around_it() {
        let (_dir, store) = store();
        store.write("a.md", "# A\n\nabout a", false).unwrap();
        fs::write(
            store.root().join("README.md"),
            "# Tài liệu\n\nĐoạn mở đầu do người viết.\n",
        )
        .unwrap();

        store.rebuild_index().unwrap();
        let first = fs::read_to_string(store.root().join("README.md")).unwrap();
        assert!(first.contains("Đoạn mở đầu do người viết."));
        assert!(first.contains("[A](a.md)"));

        // A second document appears, and the hand-written prose is still there.
        store.write("b.md", "# B\n\nabout b", false).unwrap();
        store.rebuild_index().unwrap();
        let second = fs::read_to_string(store.root().join("README.md")).unwrap();
        assert!(second.contains("Đoạn mở đầu do người viết."));
        assert!(second.contains("[B](b.md)"));
        // Rebuilt, not appended twice.
        assert_eq!(second.matches("[A](a.md)").count(), 1);
        assert_eq!(second.matches(INDEX_BEGIN).count(), 1);
    }

    #[test]
    fn a_deleted_document_leaves_the_index() {
        let (_dir, store) = store();
        store.write("a.md", "# A\n\nabout a", false).unwrap();
        store.write("b.md", "# B\n\nabout b", false).unwrap();
        store.rebuild_index().unwrap();
        fs::remove_file(store.root().join("b.md")).unwrap();
        store.rebuild_index().unwrap();
        let idx = fs::read_to_string(store.root().join("README.md")).unwrap();
        assert!(idx.contains("[A](a.md)"));
        assert!(!idx.contains("[B](b.md)"));
    }

    #[test]
    fn the_docs_language_is_read_off_the_project_not_configured() {
        let (_dir, store) = store();
        assert_eq!(store.language(), DocsLanguage::Unknown);
        store
            .write("a.md", "# Kiến trúc\n\nMô tả luồng dữ liệu.", false)
            .unwrap();
        assert_eq!(store.language(), DocsLanguage::Vietnamese);

        let (_d2, english) = store2();
        english
            .write("a.md", "# Architecture\n\nHow data flows.", false)
            .unwrap();
        assert_eq!(english.language(), DocsLanguage::English);
    }

    fn store2() -> (tempfile::TempDir, DocsStore) {
        store()
    }

    #[test]
    fn a_project_without_docs_is_not_silently_given_one() {
        let (_dir, store) = store();
        assert!(!store.exists());
        // Rebuilding an index where there is no docs/ directory creates
        // nothing: adopting the convention is the user's call.
        assert_eq!(store.rebuild_index().unwrap(), None);
        assert!(!store.root().exists());
    }

    #[test]
    fn a_slug_folds_vietnamese_rather_than_escaping_it() {
        assert_eq!(
            slugify("Chuyển tool thành bước"),
            "chuyen-tool-thanh-buoc"
        );
        assert_eq!(slugify("  Fix   the   thing!  "), "fix-the-thing");
        // Nothing usable in the title still yields a valid file name.
        assert_eq!(slugify("！！！"), "ghi-chu");
        assert!(slugify(&"rất dài ".repeat(40)).len() <= 60);
    }
}

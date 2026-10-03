//! Repo map — a compact, ranked outline of the repository the chat is
//! working in, plus symbol lookups built on the same index.
//!
//! Aider's idea: parse every source file with tree-sitter, keep the
//! definitions and references its grammar's `tags.scm` names, rank files by
//! PageRank over the reference graph (personalized toward the files the
//! conversation mentions), and show the model the best few thousand tokens
//! of signatures. The model then knows *where* things are before it greps.
//!
//! Design:
//! - **One index per workspace, process-wide**, refreshed incrementally by
//!   mtime/size; persisted as JSON under `~/.senclaw/repo-map/` so a daemon
//!   restart does not re-parse a large tree.
//! - **Never blocks a turn.** [`map_for_prompt`] answers from the cache; when
//!   the index is missing or stale it kicks a background refresh and returns
//!   what it has (nothing, on the very first turn).
//! - **Only project directories.** A working directory is mapped when it is a
//!   git repository or carries a project manifest; `$HOME` is never crawled.
//! - Off switch: `SENCLAW_REPO_MAP_TOKENS=0` (default budget 2000).

pub mod lang;
pub mod parse;
pub mod rank;
pub mod render;
pub mod scan;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use rank::RankedDef;

/// Default token budget for the prompt block.
pub const DEFAULT_BUDGET_TOKENS: usize = 2000;
/// An index older than this is refreshed (in the background) on next use.
const STALE_AFTER: Duration = Duration::from_secs(45);
/// Manifests that make a non-git folder a project worth mapping.
const PROJECT_MARKERS: &[&str] = &[
    "Cargo.toml", "package.json", "pyproject.toml", "setup.py", "go.mod", "pubspec.yaml",
    "pom.xml", "build.gradle", "CMakeLists.txt", "Makefile", "composer.json", "Gemfile",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Def {
    pub name: String,
    /// `function` | `method` | `class` | `interface` | `module` | `type` | …
    pub kind: String,
    pub line: u32,
    pub end_line: u32,
    /// First line of the definition, trimmed.
    pub sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub name: String,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub mtime: u64,
    pub size: u64,
    pub lang: String,
    pub defs: Vec<Def>,
    pub refs: Vec<Reference>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub root: PathBuf,
    /// Relative path → parsed entry.
    pub files: HashMap<String, FileEntry>,
    /// Unix seconds of the last completed refresh.
    pub built_at: u64,
}

impl Index {
    pub fn new(root: PathBuf) -> Self {
        Self { root, files: HashMap::new(), built_at: 0 }
    }

    /// Definitions whose name matches (exact, or contains when `fuzzy`),
    /// optionally filtered by kind. Returns `(file, def)` pairs.
    pub fn find_symbol(&self, name: &str, kind: Option<&str>, fuzzy: bool) -> Vec<(&str, &Def)> {
        let needle = name.to_lowercase();
        let mut out: Vec<(&str, &Def)> = self
            .files
            .iter()
            .flat_map(|(f, e)| e.defs.iter().map(move |d| (f.as_str(), d)))
            .filter(|(_, d)| {
                let n = d.name.to_lowercase();
                (if fuzzy { n.contains(&needle) } else { n == needle })
                    && kind.map(|k| d.kind == k).unwrap_or(true)
            })
            .collect();
        out.sort_by(|a, b| (a.0, a.1.line).cmp(&(b.0, b.1.line)));
        out
    }

    /// Files referencing `name`, with the lines. Files that only *define*
    /// it are excluded.
    pub fn find_references(&self, name: &str) -> Vec<(&str, Vec<u32>)> {
        let mut out: Vec<(&str, Vec<u32>)> = self
            .files
            .iter()
            .filter_map(|(f, e)| {
                let lines: Vec<u32> = e.refs.iter().filter(|r| r.name == name).map(|r| r.line).collect();
                if lines.is_empty() {
                    None
                } else {
                    Some((f.as_str(), lines))
                }
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(b.0));
        out
    }

    /// Bring the index up to date with the tree: reparse files whose
    /// mtime/size changed, drop files that are gone. Blocking; call from a
    /// blocking task.
    pub fn refresh(&mut self) -> RefreshStats {
        let started = Instant::now();
        let listed = scan::list_files(&self.root);
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut stats = RefreshStats::default();
        for meta in listed {
            seen.insert(meta.rel.clone());
            let unchanged = self
                .files
                .get(&meta.rel)
                .map(|e| e.mtime == meta.mtime && e.size == meta.size)
                .unwrap_or(false);
            if unchanged {
                stats.unchanged += 1;
                continue;
            }
            if meta.size > parse::MAX_PARSE_BYTES {
                stats.skipped += 1;
                continue;
            }
            let abs = self.root.join(&meta.rel);
            let Ok(bytes) = std::fs::read(&abs) else { continue };
            let src = String::from_utf8_lossy(&bytes);
            let Some(parsed) = parse::parse_file(&meta.rel, &src) else {
                stats.skipped += 1;
                continue;
            };
            let lang = lang::spec_for_path(&meta.rel).map(|s| s.id).unwrap_or("").to_string();
            self.files.insert(
                meta.rel,
                FileEntry { mtime: meta.mtime, size: meta.size, lang, defs: parsed.defs, refs: parsed.refs },
            );
            stats.parsed += 1;
        }
        let before = self.files.len();
        self.files.retain(|f, _| seen.contains(f));
        stats.removed = before - self.files.len();
        self.built_at = now_secs();
        stats.elapsed = started.elapsed();
        stats
    }

    /// The map text for a prompt: ranked, personalized toward `focus`
    /// (relative paths), cut to `budget_tokens`.
    pub fn render(&self, focus: &[String], budget_tokens: usize) -> String {
        let ranked = rank::rank(self, focus);
        render::render(self, &ranked, budget_tokens)
    }

    /// Relative paths mentioned verbatim in `text` (so a prompt that names
    /// `src/lib.rs` personalizes the map toward it).
    pub fn focus_from_text(&self, text: &str) -> Vec<String> {
        if self.files.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for tok in text.split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | ',' | ';' | '<' | '>')) {
            let tok = tok.trim_start_matches('@').trim_start_matches("./");
            if tok.len() < 4 || !tok.contains('.') {
                continue;
            }
            if self.files.contains_key(tok) {
                out.push(tok.to_string());
                continue;
            }
            // A bare file name matches when it is unique in the tree.
            let mut hits = self.files.keys().filter(|f| f.ends_with(&format!("/{tok}")) || f.as_str() == tok);
            if let (Some(h), None) = (hits.next(), hits.next()) {
                out.push(h.clone());
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

#[derive(Debug, Default)]
pub struct RefreshStats {
    pub parsed: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub removed: usize,
    pub elapsed: Duration,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Is this directory a project we should map? Never `$HOME` or `/`.
pub fn is_project_dir(dir: &Path) -> bool {
    if !dir.is_absolute() || !dir.is_dir() {
        return false;
    }
    if dir == Path::new("/") || dirs::home_dir().as_deref() == Some(dir) {
        return false;
    }
    dir.join(".git").exists() || PROJECT_MARKERS.iter().any(|m| dir.join(m).exists())
}

/// Token budget from the environment; `0` disables the prompt block.
pub fn budget_tokens() -> usize {
    std::env::var("SENCLAW_REPO_MAP_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_BUDGET_TOKENS)
}

// ===== process-wide cache =====

struct Slot {
    index: Arc<Mutex<Index>>,
    /// Set while a background refresh runs, so two turns do not start two.
    refreshing: Arc<std::sync::atomic::AtomicBool>,
    last_refresh: Mutex<Option<Instant>>,
}

static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<Slot>>>> = OnceLock::new();

fn cache_dir() -> PathBuf {
    crate::util::paths::senclaw_home()
        .join("repo-map")
}

fn cache_file(root: &Path) -> PathBuf {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in root.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    cache_dir().join(format!("{h:016x}.json"))
}

fn load_from_disk(root: &Path) -> Option<Index> {
    let raw = std::fs::read(cache_file(root)).ok()?;
    let idx: Index = serde_json::from_slice(&raw).ok()?;
    if idx.root == root {
        Some(idx)
    } else {
        None
    }
}

fn save_to_disk(index: &Index) {
    let path = cache_file(&index.root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec(index) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn slot_for(root: &Path) -> Arc<Slot> {
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap();
    guard
        .entry(root.to_path_buf())
        .or_insert_with(|| {
            let index = load_from_disk(root).unwrap_or_else(|| Index::new(root.to_path_buf()));
            Arc::new(Slot {
                index: Arc::new(Mutex::new(index)),
                refreshing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_refresh: Mutex::new(None),
            })
        })
        .clone()
}

/// Refresh in the background unless one is already running or the index is
/// fresh. Returns `true` when a refresh was started.
fn kick_refresh(slot: &Arc<Slot>, force: bool) -> bool {
    use std::sync::atomic::Ordering;
    if !force {
        if let Some(t) = *slot.last_refresh.lock().unwrap() {
            if t.elapsed() < STALE_AFTER {
                return false;
            }
        }
    }
    if slot.refreshing.swap(true, Ordering::SeqCst) {
        return false;
    }
    let slot2 = Arc::clone(slot);
    tokio::task::spawn_blocking(move || {
        // Work on a clone so readers are never blocked behind a parse.
        let mut working = slot2.index.lock().unwrap().clone();
        let stats = working.refresh();
        save_to_disk(&working);
        *slot2.index.lock().unwrap() = working;
        *slot2.last_refresh.lock().unwrap() = Some(Instant::now());
        slot2.refreshing.store(false, Ordering::SeqCst);
        if stats.parsed > 0 || stats.removed > 0 {
            tracing::info!(
                parsed = stats.parsed,
                unchanged = stats.unchanged,
                removed = stats.removed,
                ms = stats.elapsed.as_millis() as u64,
                "[RepoMap] index refreshed"
            );
        }
    });
    true
}

/// Snapshot of the workspace index for tools. Kicks a background refresh
/// when stale; the returned index may be a moment behind the disk.
pub fn index_for(working_dir: &str) -> Option<Arc<Mutex<Index>>> {
    let dir = crate::util::paths::expand_tilde(working_dir);
    if !is_project_dir(&dir) {
        return None;
    }
    let slot = slot_for(&dir);
    kick_refresh(&slot, false);
    Some(Arc::clone(&slot.index))
}

/// Block until the index for `working_dir` is refreshed. For tools that
/// would rather wait a second than answer from nothing on a first call.
pub async fn index_ready(working_dir: &str) -> Option<Arc<Mutex<Index>>> {
    let dir = crate::util::paths::expand_tilde(working_dir);
    if !is_project_dir(&dir) {
        return None;
    }
    let slot = slot_for(&dir);
    let empty = slot.index.lock().unwrap().files.is_empty();
    if empty || slot.last_refresh.lock().unwrap().is_none() {
        // First use: refresh synchronously (on the blocking pool) so the
        // symbol tools answer correctly instead of "unknown" on turn one.
        let idx = Arc::clone(&slot.index);
        let root = dir.clone();
        let refreshed = tokio::task::spawn_blocking(move || {
            let mut working = idx.lock().unwrap().clone();
            working.refresh();
            save_to_disk(&working);
            working.root = root;
            working
        })
        .await
        .ok()?;
        *slot.index.lock().unwrap() = refreshed;
        *slot.last_refresh.lock().unwrap() = Some(Instant::now());
    } else {
        kick_refresh(&slot, false);
    }
    Some(Arc::clone(&slot.index))
}

/// The `<repo_map>` block for a turn, or `None` when the directory is not a
/// project, the budget is 0, or the index is not built yet (a refresh is
/// started so the next turn has it).
pub fn map_for_prompt(working_dir: &str, prompt: &str) -> Option<String> {
    let budget = budget_tokens();
    if budget == 0 {
        return None;
    }
    let index = index_for(working_dir)?;
    let guard = index.lock().unwrap();
    if guard.files.is_empty() {
        return None;
    }
    let focus = guard.focus_from_text(prompt);
    let body = guard.render(&focus, budget);
    if body.trim().is_empty() {
        return None;
    }
    Some(format!(
        "<repo_map>\nRanked outline of this repository ({} files indexed; signatures only, `│line│`). \
Use it to pick where to look; read a file before editing it. `find_symbol`, \
`find_references` and `symbol_body` query the same index.\n\n{}</repo_map>",
        guard.files.len(),
        body
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            "pub mod util;\npub fn run() { util::helper(); util::helper(); }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("src/util.rs"),
            "pub fn helper() {}\npub struct Cfg { pub a: u32 }\n",
        )
        .unwrap();
        tmp
    }

    #[test]
    fn refresh_indexes_and_incremental_reparse_only_changed_files() {
        let tmp = project();
        let mut idx = Index::new(tmp.path().to_path_buf());
        let s = idx.refresh();
        assert_eq!(s.parsed, 2);
        assert!(idx.find_symbol("helper", None, false).len() == 1);
        assert_eq!(idx.find_references("helper")[0].0, "src/lib.rs");
        let s2 = idx.refresh();
        assert_eq!(s2.parsed, 0);
        assert_eq!(s2.unchanged, 2);
        std::fs::remove_file(tmp.path().join("src/util.rs")).unwrap();
        let s3 = idx.refresh();
        assert_eq!(s3.removed, 1);
        assert!(idx.find_symbol("helper", None, false).is_empty());
    }

    #[test]
    fn render_and_focus() {
        let tmp = project();
        let mut idx = Index::new(tmp.path().to_path_buf());
        idx.refresh();
        let text = idx.render(&[], 500);
        assert!(text.contains("src/util.rs:"));
        assert!(text.contains("pub fn helper()"));
        let focus = idx.focus_from_text("please look at src/lib.rs and util.rs");
        assert_eq!(focus, vec!["src/lib.rs".to_string(), "src/util.rs".to_string()]);
        assert!(idx.render(&[], 0).is_empty());
    }

    #[test]
    fn project_detection_refuses_home_and_plain_folders() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_project_dir(tmp.path()));
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        assert!(is_project_dir(tmp.path()));
        if let Some(h) = dirs::home_dir() {
            assert!(!is_project_dir(&h));
        }
        assert!(!is_project_dir(Path::new("/")));
    }
}

#[cfg(test)]
mod bench {
    //! `cargo test --lib repo_map::bench -- --ignored --nocapture` — indexes
    //! this repository itself and prints cold/warm timings and map size.
    use super::*;

    #[test]
    #[ignore]
    fn index_this_repository() {
        let root = std::env::current_dir().unwrap();
        let mut idx = Index::new(root);
        let cold = idx.refresh();
        let warm = idx.refresh();
        let t = Instant::now();
        let text = idx.render(&["src/lib.rs".into()], DEFAULT_BUDGET_TOKENS);
        let render_ms = t.elapsed().as_millis();
        let tokens = crate::memory::chunker::estimate_tokens(&text);
        println!(
            "files={} cold={}ms (parsed {}) warm={}ms render={}ms map_chars={} map_tokens≈{}",
            idx.files.len(),
            cold.elapsed.as_millis(),
            cold.parsed,
            warm.elapsed.as_millis(),
            render_ms,
            text.len(),
            tokens
        );
        println!("{}", text.lines().take(40).collect::<Vec<_>>().join("\n"));
        assert!(cold.parsed > 100);
        assert!(warm.elapsed.as_secs() <= 2, "warm refresh must be fast");
    }
}

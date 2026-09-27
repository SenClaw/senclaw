//! Which files belong to the map.
//!
//! In a git repository the answer is git's: `ls-files` with untracked-but-not-
//! ignored files added, so the project's `.gitignore` is honoured exactly.
//! Elsewhere a plain walk with the usual dependency/output directories
//! skipped. Either way only files with a supported grammar are returned.

use std::path::{Path, PathBuf};

use super::lang::spec_for_path;

/// Directories never worth mapping, whatever the walk mode.
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".dart_tool",
    "build",
    "dist",
    ".next",
    ".cache",
    "vendor",
    ".idea",
    ".vscode",
];

/// Hard ceiling on files per map. A monorepo past this is mapped partially
/// (first N in walk order) rather than making every turn wait.
pub const MAX_FILES: usize = 20_000;

pub struct FileMeta {
    /// Relative to the workspace root, `/`-separated.
    pub rel: String,
    pub mtime: u64,
    pub size: u64,
}

fn mtime_secs(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn git_listed(root: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    if out.len() >= MAX_FILES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                walk(root, &p, out);
            }
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
            if out.len() >= MAX_FILES {
                return;
            }
        }
    }
}

/// Supported source files under `root`, with the metadata the index uses to
/// decide what to reparse.
pub fn list_files(root: &Path) -> Vec<FileMeta> {
    let rels = if root.join(".git").exists() {
        git_listed(root).unwrap_or_else(|| {
            let mut v = Vec::new();
            walk(root, root, &mut v);
            v
        })
    } else {
        let mut v = Vec::new();
        walk(root, root, &mut v);
        v
    };
    let mut out = Vec::new();
    for rel in rels.into_iter().take(MAX_FILES) {
        if spec_for_path(&rel).is_none() {
            continue;
        }
        // git may list files inside skipped dirs when they are tracked (a
        // committed `vendor/`); the map still does not want them.
        if rel.split('/').any(|c| SKIP_DIRS.contains(&c)) {
            continue;
        }
        let abs: PathBuf = root.join(&rel);
        let Ok(m) = std::fs::metadata(&abs) else { continue };
        if !m.is_file() {
            continue;
        }
        out.push(FileMeta {
            rel,
            mtime: mtime_secs(&m),
            size: m.len(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_skips_dependency_dirs_and_unsupported_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/x")).unwrap();
        std::fs::write(tmp.path().join("src/a.rs"), "fn a() {}").unwrap();
        std::fs::write(tmp.path().join("src/b.md"), "# b").unwrap();
        std::fs::write(tmp.path().join("node_modules/x/i.js"), "x").unwrap();
        let files = list_files(tmp.path());
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["src/a.rs"]);
    }
}

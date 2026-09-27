//! File access inside a sandbox's directory.
//!
//! Both backends keep the sandbox's files on the host (docker bind-mounts the
//! same directory at `/work`), so the UI and the MCP tools read and write them
//! directly instead of shelling into the sandbox. That makes the path check
//! here the whole boundary: it is the only thing standing between
//! `sbx_file_write(path="../../../../.ssh/authorized_keys")` and the user's
//! real home directory.

use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::Serialize;

/// Largest file the API will read back inline.
pub const MAX_READ: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub dir: bool,
    pub size: i64,
    pub modified_ms: i64,
}

/// Where file operations are allowed to reach.
///
/// The sandbox root, plus the real paths of any folders the user explicitly
/// mounted. Mounts have to be listed separately because on macOS they are
/// symlinks pointing out of the sandbox — exactly the shape the escape check
/// exists to reject — so the check needs to know which few destinations were
/// deliberate.
#[derive(Debug, Clone)]
pub struct Scope {
    pub root: PathBuf,
    pub mounts: Vec<PathBuf>,
}

impl Scope {
    pub fn of(sb: &crate::sandbox::db::Sandbox) -> Self {
        Scope {
            root: PathBuf::from(&sb.workdir),
            mounts: sb.mounts.iter().map(|m| PathBuf::from(&m.source)).collect(),
        }
    }

    #[cfg(test)]
    pub fn bare(root: &Path) -> Self {
        Scope {
            root: root.to_path_buf(),
            mounts: Vec::new(),
        }
    }

    fn permits(&self, real: &Path) -> bool {
        let root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
        if real.starts_with(&root) {
            return true;
        }
        self.mounts.iter().any(|m| {
            let m = m.canonicalize().unwrap_or_else(|_| m.clone());
            real.starts_with(&m)
        })
    }
}

/// Resolve a caller-supplied relative path against the sandbox root.
///
/// Rejects absolute paths and any `..` that climbs out. The check is *lexical
/// first* — `..` components are resolved against the accumulated path and the
/// result must stay under the root — so it works for paths that do not exist
/// yet, which `canonicalize` cannot do. When the path (or its parent) does
/// exist, the resolved real path is checked too, which is what catches a
/// symlink inside the sandbox pointing outward.
pub fn resolve(scope: &Scope, rel: &str) -> Result<PathBuf> {
    let root = &scope.root;
    let rel = rel.trim().trim_start_matches("./");
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(anyhow!("path must be relative to the sandbox: `{rel}`"));
    }

    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(anyhow!("path `{rel}` escapes the sandbox"));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!("path must be relative to the sandbox: `{rel}`"));
            }
        }
    }

    let full = root.join(&out);

    // Symlink check. The deepest existing ancestor is canonicalized and must
    // land somewhere the scope permits — a plain string prefix test would
    // happily accept `<root>/link` where `link -> /Users/you`.
    let mut probe = full.clone();
    loop {
        if let Ok(real) = probe.canonicalize() {
            if !scope.permits(&real) {
                return Err(anyhow!("path `{rel}` points outside the sandbox (symlink)"));
            }
            break;
        }
        if !probe.pop() || probe.as_os_str().is_empty() {
            break;
        }
    }

    Ok(full)
}

pub fn list(scope: &Scope, rel: &str) -> Result<Vec<Entry>> {
    let dir = resolve(scope, rel)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir)? {
        let e = e?;
        let md = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let name = e.file_name().to_string_lossy().to_string();
        // Internal bookkeeping the app itself created — showing it invites the
        // user to edit the Seatbelt profile from the file browser.
        if name == ".sandbox-profile.sb" {
            continue;
        }
        let path = if rel.trim().is_empty() || rel == "." {
            name.clone()
        } else {
            format!("{}/{}", rel.trim_end_matches('/'), name)
        };
        out.push(Entry {
            name,
            path,
            dir: md.is_dir(),
            size: md.len() as i64,
            modified_ms: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        });
    }
    out.sort_by(|a, b| b.dir.cmp(&a.dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(out)
}

pub fn read(scope: &Scope, rel: &str) -> Result<String> {
    let p = resolve(scope, rel)?;
    let md = std::fs::metadata(&p).map_err(|_| anyhow!("no such file `{rel}`"))?;
    if md.is_dir() {
        return Err(anyhow!("`{rel}` is a directory"));
    }
    if md.len() > MAX_READ {
        return Err(anyhow!(
            "file `{rel}` is {} bytes, over the {MAX_READ} byte read limit",
            md.len()
        ));
    }
    let bytes = std::fs::read(&p)?;
    // Binary content is reported as such rather than mangled into replacement
    // characters that the caller would then write back and corrupt the file.
    match String::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(_) => Err(anyhow!("`{rel}` is not UTF-8 text (binary file)")),
    }
}

pub fn write(scope: &Scope, rel: &str, content: &str) -> Result<u64> {
    let p = resolve(scope, rel)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&p, content)?;
    Ok(content.len() as u64)
}

pub fn delete(scope: &Scope, rel: &str) -> Result<()> {
    let p = resolve(scope, rel)?;
    if rel.trim().is_empty() || rel == "." {
        return Err(anyhow!("cannot delete the sandbox root"));
    }
    if p.is_dir() {
        std::fs::remove_dir_all(p)?;
    } else {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

pub fn mkdir(scope: &Scope, rel: &str) -> Result<()> {
    let p = resolve(scope, rel)?;
    std::fs::create_dir_all(p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn plain_relative_paths_resolve_under_the_root() {
        let d = root();
        let p = resolve(&Scope::bare(d.path()), "a/b.txt").unwrap();
        assert!(p.starts_with(d.path()));
        assert!(p.ends_with("a/b.txt"));
    }

    #[test]
    fn dotdot_cannot_climb_out() {
        let d = root();
        for bad in ["../escape", "a/../../escape", "../../../../etc/passwd", ".."] {
            assert!(resolve(&Scope::bare(d.path()), bad).is_err(), "`{bad}` must be rejected");
        }
    }

    #[test]
    fn dotdot_that_stays_inside_is_allowed() {
        let d = root();
        let p = resolve(&Scope::bare(d.path()), "a/b/../c.txt").unwrap();
        assert!(p.ends_with("a/c.txt"));
    }

    #[test]
    fn absolute_paths_are_rejected() {
        let d = root();
        assert!(resolve(&Scope::bare(d.path()), "/etc/passwd").is_err());
        assert!(resolve(&Scope::bare(d.path()), "/").is_err());
    }

    #[test]
    fn a_symlink_pointing_outside_is_rejected() {
        let d = root();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s3cret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), d.path().join("link")).unwrap();
        // Lexically this is inside the root; only the canonicalize check catches it.
        assert!(resolve(&Scope::bare(d.path()), "link/secret.txt").is_err());
    }

    #[test]
    fn write_then_read_round_trips_including_non_ascii() {
        let d = root();
        write(&Scope::bare(d.path()), "thư/mục/ghi chú.txt", "xin chào ✅").unwrap();
        assert_eq!(read(&Scope::bare(d.path()), "thư/mục/ghi chú.txt").unwrap(), "xin chào ✅");
    }

    #[test]
    fn reading_a_binary_file_reports_it_instead_of_mangling_it() {
        let d = root();
        std::fs::write(d.path().join("bin"), [0xff, 0xfe, 0x00]).unwrap();
        let e = read(&Scope::bare(d.path()), "bin").unwrap_err().to_string();
        assert!(e.contains("binary"));
    }

    #[test]
    fn oversized_files_are_refused_with_the_limit_named() {
        let d = root();
        std::fs::write(d.path().join("big"), vec![b'a'; (MAX_READ + 1) as usize]).unwrap();
        let e = read(&Scope::bare(d.path()), "big").unwrap_err().to_string();
        assert!(e.contains("read limit"));
    }

    #[test]
    fn listing_hides_the_generated_sandbox_profile() {
        let d = root();
        std::fs::write(d.path().join(".sandbox-profile.sb"), "(version 1)").unwrap();
        write(&Scope::bare(d.path()), "keep.txt", "x").unwrap();
        let names: Vec<_> = list(&Scope::bare(d.path()), "").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["keep.txt"]);
    }

    #[test]
    fn listing_puts_directories_first() {
        let d = root();
        write(&Scope::bare(d.path()), "z.txt", "x").unwrap();
        mkdir(&Scope::bare(d.path()), "adir").unwrap();
        let e = list(&Scope::bare(d.path()), "").unwrap();
        assert!(e[0].dir);
        assert_eq!(e[0].name, "adir");
    }

    #[test]
    fn listing_a_missing_directory_is_empty_not_an_error() {
        let d = root();
        assert!(list(&Scope::bare(d.path()), "nope").unwrap().is_empty());
    }

    #[test]
    fn the_sandbox_root_itself_cannot_be_deleted() {
        let d = root();
        assert!(delete(&Scope::bare(d.path()), "").is_err());
        assert!(delete(&Scope::bare(d.path()), ".").is_err());
        assert!(d.path().exists());
    }

    #[test]
    fn delete_removes_files_and_trees() {
        let d = root();
        write(&Scope::bare(d.path()), "sub/a.txt", "x").unwrap();
        delete(&Scope::bare(d.path()), "sub").unwrap();
        assert!(!d.path().join("sub").exists());
    }
}

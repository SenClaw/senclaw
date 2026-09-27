//! Binding a folder from the real machine into a sandbox.
//!
//! A mount is `source` (a real path on this machine) appearing at `target` (a
//! relative path inside the sandbox). Both backends put it in the same place —
//! `<sandbox root>/<target>` — so a snippet written against one backend works on
//! the other, and the file browser can walk into it either way.
//!
//! How each backend gets there differs:
//!
//! * **docker** — a real bind mount, `-v source:/work/target[:ro]`.
//! * **bubblewrap** — a real bind mount, `--bind` / `--ro-bind`.
//! * **Seatbelt** — macOS cannot remap paths for a process, so the mount is a
//!   **symlink** at `<workdir>/<target>` pointing at `source`, plus a rule in
//!   the sandbox profile granting access to `source`. The path inside the
//!   sandbox is the same; what differs is that the sandboxed code can also
//!   still see the real path, because there is no namespace hiding it.
//!
//! ## The guard list
//!
//! Mounting is the one feature here that deliberately punches a hole in the
//! sandbox, so what may be mounted is restricted. Mounting `/` read-write would
//! turn the app into a no-op with extra steps; mounting `~/.ssh` hands over the
//! keys the sandbox otherwise blocks; mounting the app's own workspace root
//! lets one sandbox rewrite another's files, including the Seatbelt profile a
//! future run will be launched with.

use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::sandbox::config;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Mount {
    /// Absolute path on the real machine.
    pub source: String,
    /// Where it appears inside the sandbox, relative to the sandbox root.
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

/// Absolute paths that may never be a mount source, whatever the user asks.
/// Prefix match, so `/etc/anything` is covered by `/etc`.
fn forbidden_roots() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ["/", "/System", "/usr", "/bin", "/sbin", "/etc", "/var", "/private/var", "/Library", "/dev", "/proc"]
        .iter()
        .map(PathBuf::from)
        .collect();
    // The app's own data: mounting it would expose every other sandbox, and let
    // one sandbox rewrite the Seatbelt profile another is about to be run with.
    v.push(config::data_dir());
    if let Ok(home) = std::env::var("HOME") {
        let h = PathBuf::from(&home);
        for secret in [
            ".ssh",
            ".aws",
            ".gnupg",
            ".config/gcloud",
            ".kube",
            ".docker",
            ".senclaw",
            "Library/Keychains",
        ] {
            v.push(h.join(secret));
        }
        // The home directory itself — a read-write mount of it is the same as
        // no sandbox at all. A named subfolder is fine.
        v.push(h);
    }
    v
}

/// Validate and normalise a mount request.
pub fn validate(source: &str, target: &str, read_only: bool) -> Result<Mount> {
    let source = source.trim();
    if source.is_empty() {
        return Err(anyhow!("missing the host folder path"));
    }
    let src = PathBuf::from(source);
    if !src.is_absolute() {
        return Err(anyhow!("the host path must be absolute: `{source}`"));
    }
    // Resolve before checking, so `/Users/you/../../etc` cannot walk past the
    // guard list, and so a symlinked source is judged by where it really goes.
    let real = src
        .canonicalize()
        .map_err(|_| anyhow!("`{source}` does not exist on this machine"))?;
    if !real.is_dir() {
        return Err(anyhow!("`{source}` is not a directory"));
    }

    for bad in forbidden_roots() {
        let bad = bad.canonicalize().unwrap_or(bad);
        if real == bad {
            return Err(anyhow!(
                "mounting `{}` is not allowed — pick a specific subfolder instead of the whole thing",
                real.display()
            ));
        }
        if real.starts_with(&bad) && is_secret(&bad) {
            return Err(anyhow!(
                "mounting `{}` is not allowed: it sits inside `{}` (a folder holding sensitive data)",
                real.display(),
                bad.display()
            ));
        }
    }

    let target = normalise_target(target, &real)?;
    Ok(Mount {
        source: real.to_string_lossy().to_string(),
        target,
        read_only,
    })
}

/// A forbidden root that is forbidden *including its children*, as opposed to
/// one where only the root itself is refused. `/usr/local/src` is a reasonable
/// thing to mount; `~/.ssh/somewhere` never is.
fn is_secret(p: &Path) -> bool {
    let s = p.to_string_lossy();
    [".ssh", ".aws", ".gnupg", "gcloud", ".kube", ".docker", ".senclaw", "Keychains"]
        .iter()
        .any(|k| s.contains(k))
        || p.starts_with(config::data_dir())
}

/// Target must be a plain relative path inside the sandbox. Defaults to the
/// source's own folder name, which is what people mean nine times in ten.
fn normalise_target(target: &str, source: &Path) -> Result<String> {
    let raw = target.trim();
    // An absolute target is refused rather than quietly stripped to a relative
    // one. Someone writing `/data` means "mount it at /data in the container";
    // they would actually get `/work/data`, and silently rewriting the path is
    // how that misunderstanding survives until their script cannot find it.
    if raw.starts_with('/') {
        return Err(anyhow!(
            "`target` is a path RELATIVE to the sandbox — write `{}` instead of `{raw}`",
            raw.trim_start_matches('/')
        ));
    }
    let t = raw.trim_start_matches("./").trim_matches('/');
    if t.is_empty() {
        return source
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .ok_or_else(|| anyhow!("cannot derive a target name — set `target` explicitly"));
    }
    let p = Path::new(t);
    if p.is_absolute() {
        return Err(anyhow!("`target` must be a path relative to the sandbox"));
    }
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            _ => {
                return Err(anyhow!(
                    "`target` must be a folder name inside the sandbox — no `..` and no leading `/`"
                ))
            }
        }
    }
    // The app's own bookkeeping directories.
    if t.starts_with(".runs") || t.starts_with(".tmp") || t == ".sandbox-profile.sb" {
        return Err(anyhow!("`{t}` is an internal sandbox folder — choose another name"));
    }
    Ok(t.to_string())
}

/// Reject a second mount at a target already taken.
pub fn add(existing: &[Mount], m: Mount) -> Result<Vec<Mount>> {
    if existing.iter().any(|e| e.target == m.target) {
        return Err(anyhow!("a folder is already mounted at `{}`", m.target));
    }
    let mut v = existing.to_vec();
    v.push(m);
    Ok(v)
}

pub fn remove(existing: &[Mount], target: &str) -> Vec<Mount> {
    existing.iter().filter(|m| m.target != target).cloned().collect()
}

/// Create the Seatbelt-mode symlinks for a sandbox's mounts.
///
/// Only used on macOS, where there is no bind mount to make. Idempotent: an
/// existing correct link is left alone, a stale one is replaced.
pub fn materialise_symlinks(workdir: &Path, mounts: &[Mount]) -> Result<()> {
    for m in mounts {
        let link = workdir.join(&m.target);
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::read_link(&link) {
            Ok(cur) if cur == Path::new(&m.source) => continue,
            Ok(_) => {
                std::fs::remove_file(&link)?;
            }
            Err(_) if link.exists() => {
                return Err(anyhow!(
                    "`{}` already exists in the sandbox and is not a link — choose another `target`",
                    m.target
                ))
            }
            Err(_) => {}
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&m.source, &link)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_normal_folder_is_accepted_and_named_after_itself() {
        let d = tmp();
        let sub = d.path().join("duan");
        std::fs::create_dir(&sub).unwrap();
        let m = validate(sub.to_str().unwrap(), "", false).unwrap();
        assert_eq!(m.target, "duan");
        assert!(!m.read_only);
    }

    #[test]
    fn the_home_directory_itself_is_refused() {
        let home = std::env::var("HOME").unwrap();
        let e = validate(&home, "h", false).unwrap_err().to_string();
        assert!(e.contains("is not allowed"), "got: {e}");
    }

    #[test]
    fn root_and_system_directories_are_refused() {
        for p in ["/", "/etc", "/usr"] {
            assert!(validate(p, "x", true).is_err(), "`{p}` must be refused");
        }
    }

    #[test]
    fn credential_directories_are_refused_including_children() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let ssh = home.join(".ssh");
        if ssh.is_dir() {
            assert!(validate(ssh.to_str().unwrap(), "k", true).is_err());
        }
        // And the app's own data root, whatever it holds.
        let data = config::data_dir();
        std::fs::create_dir_all(&data).ok();
        if data.is_dir() {
            assert!(
                validate(data.to_str().unwrap(), "d", true).is_err(),
                "the app's own data dir must not be mountable"
            );
        }
    }

    #[test]
    fn a_dotdot_source_cannot_walk_past_the_guard_list() {
        let home = std::env::var("HOME").unwrap();
        // Resolves to the home directory, which is refused — a purely lexical
        // check would have let this through.
        let sneaky = format!("{home}/./");
        assert!(validate(&sneaky, "x", false).is_err());
    }

    #[test]
    fn a_missing_source_says_so_rather_than_failing_later() {
        let e = validate("/definitely/not/here", "x", false).unwrap_err().to_string();
        assert!(e.contains("does not exist"));
    }

    #[test]
    fn a_file_is_not_a_folder() {
        let d = tmp();
        let f = d.path().join("a.txt");
        std::fs::write(&f, "x").unwrap();
        assert!(validate(f.to_str().unwrap(), "x", false).is_err());
    }

    #[test]
    fn targets_cannot_escape_or_collide_with_internals() {
        let d = tmp();
        let sub = d.path().join("ok");
        std::fs::create_dir(&sub).unwrap();
        let s = sub.to_str().unwrap();
        for bad in ["../out", "/abs", ".runs", ".tmp/x"] {
            assert!(validate(s, bad, false).is_err(), "`{bad}` must be refused");
        }
        assert_eq!(validate(s, "data/in", false).unwrap().target, "data/in");
    }

    #[test]
    fn two_mounts_cannot_share_a_target() {
        let d = tmp();
        for n in ["a", "b"] {
            std::fs::create_dir(d.path().join(n)).unwrap();
        }
        let m1 = validate(d.path().join("a").to_str().unwrap(), "shared", false).unwrap();
        let m2 = validate(d.path().join("b").to_str().unwrap(), "shared", false).unwrap();
        let list = add(&[], m1).unwrap();
        assert!(add(&list, m2).is_err());
    }

    #[test]
    fn remove_drops_only_the_named_target() {
        let d = tmp();
        for n in ["a", "b"] {
            std::fs::create_dir(d.path().join(n)).unwrap();
        }
        let list = add(
            &add(&[], validate(d.path().join("a").to_str().unwrap(), "a", false).unwrap()).unwrap(),
            validate(d.path().join("b").to_str().unwrap(), "b", false).unwrap(),
        )
        .unwrap();
        let left = remove(&list, "a");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].target, "b");
    }

    #[test]
    fn symlinks_are_created_and_are_idempotent() {
        let host = tmp();
        let work = tmp();
        let src = host.path().join("shared");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("hello.txt"), "hi").unwrap();

        let m = validate(src.to_str().unwrap(), "shared", false).unwrap();
        materialise_symlinks(work.path(), std::slice::from_ref(&m)).unwrap();
        assert_eq!(
            std::fs::read_to_string(work.path().join("shared/hello.txt")).unwrap(),
            "hi"
        );
        // Running it again must not fail on the link that already exists.
        materialise_symlinks(work.path(), &[m]).unwrap();
    }

    #[test]
    fn a_real_directory_in_the_way_is_reported_not_deleted() {
        let host = tmp();
        let work = tmp();
        let src = host.path().join("s");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(work.path().join("s")).unwrap();
        std::fs::write(work.path().join("s/keep.txt"), "precious").unwrap();

        let m = validate(src.to_str().unwrap(), "s", false).unwrap();
        assert!(materialise_symlinks(work.path(), &[m]).is_err());
        assert!(
            work.path().join("s/keep.txt").exists(),
            "existing sandbox files must not be destroyed to make room for a mount"
        );
    }
}

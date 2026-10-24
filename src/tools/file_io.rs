//! Writing a file the way an editor should: all of it or none of it, one writer
//! at a time, with a way back.
//!
//! `std::fs::write` truncates the file and then writes it. A crash, a full disk
//! or a second agent reading in between sees half a file, and two agents that
//! both read-modify-write the same file silently lose one edit. OpenHands'
//! editor writes through a temp file, serialises calls on one file and keeps an
//! `undo_edit` history; this is the same three things for `Edit` and `Write`.
//!
//! - [`write_atomic`]: sibling temp file, permissions copied, `rename` over the
//!   target. A symlink is followed to its target (renaming over the link would
//!   replace the link with a file).
//! - [`lock_for`]: one mutex per file, process-wide, held across the whole
//!   read-modify-write. Subagents and DAG workers share the process.
//! - [`record`] / [`undo`]: the last few contents of each file this process
//!   wrote. `undo` restores only when the file still holds what the edit left,
//!   so it can never roll back somebody else's later change.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

// ----------------------------------------------------------------------------
// Atomic write
// ----------------------------------------------------------------------------

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Replace `path` with `bytes` without ever exposing a partial file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let target = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    };
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let tmp = dir.join(format!(".{name}.sen-{}-{}.tmp", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));

    let created = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp);
    let mut file = match created {
        Ok(f) => f,
        // A directory we may not add files to, but a file we may write: the old way still works.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return std::fs::write(&target, bytes),
        Err(e) => return Err(e),
    };
    let finish = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Ok(meta) = std::fs::metadata(&target) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, &target)
    })();
    if finish.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    finish
}

// ----------------------------------------------------------------------------
// One writer per file
// ----------------------------------------------------------------------------

static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The mutex that serialises writers of `path`. Take it before reading the
/// file you are about to rewrite and hold it until the write is done; a
/// poisoned lock is still a lock (`unwrap_or_else(|e| e.into_inner())`).
pub fn lock_for(path: &Path) -> Arc<Mutex<()>> {
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    if map.len() > 512 {
        map.retain(|_, l| Arc::strong_count(l) > 1);
    }
    map.entry(key(path)).or_default().clone()
}

// ----------------------------------------------------------------------------
// Undo
// ----------------------------------------------------------------------------

/// Versions kept per file, and files tracked: an editing session touches a
/// handful of files, and a version is a whole copy.
const MAX_VERSIONS: usize = 8;
const MAX_FILES: usize = 64;
/// A file larger than this is not kept (generated bundles, data).
const MAX_VERSION_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
struct Version {
    /// What the file held before the write; `None` = the write created it.
    before: Option<String>,
    /// Hash of what the write left, to tell whether anyone changed it since.
    after: u64,
}

#[derive(Default)]
struct History {
    files: HashMap<PathBuf, Vec<Version>>,
    order: VecDeque<PathBuf>,
}

static HISTORY: OnceLock<Mutex<History>> = OnceLock::new();

fn hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Remember that `path` went from `before` (`None` = it did not exist) to `after`.
pub fn record(path: &Path, before: Option<&str>, after: &str) {
    if before.map_or(false, |b| b.len() > MAX_VERSION_BYTES) || after.len() > MAX_VERSION_BYTES {
        // Forget the file rather than offer an undo that skips a version.
        forget(path);
        return;
    }
    let mut h = HISTORY.get_or_init(|| Mutex::new(History::default())).lock().unwrap_or_else(|e| e.into_inner());
    let k = key(path);
    if !h.files.contains_key(&k) {
        if h.order.len() >= MAX_FILES {
            if let Some(old) = h.order.pop_front() {
                h.files.remove(&old);
            }
        }
        h.order.push_back(k.clone());
    }
    let versions = h.files.entry(k).or_default();
    versions.push(Version { before: before.map(str::to_string), after: hash(after) });
    if versions.len() > MAX_VERSIONS {
        versions.remove(0);
    }
}

fn forget(path: &Path) {
    let mut h = HISTORY.get_or_init(|| Mutex::new(History::default())).lock().unwrap_or_else(|e| e.into_inner());
    let k = key(path);
    h.files.remove(&k);
    h.order.retain(|p| p != &k);
}

/// Why an undo was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum UndoError {
    /// Nothing of this file was written by an edit in this process.
    NothingToUndo,
    /// The file no longer holds what the last edit left: someone changed it since.
    ChangedSince,
}

/// What the file should become to take back its last write: `Some(text)` to
/// restore, `None` when the write created the file (so it should be removed).
/// `current` is the file's content right now. The version is consumed only
/// when it is returned.
pub fn undo(path: &Path, current: &str) -> Result<Option<String>, UndoError> {
    let mut h = HISTORY.get_or_init(|| Mutex::new(History::default())).lock().unwrap_or_else(|e| e.into_inner());
    let k = key(path);
    let versions = h.files.get_mut(&k).filter(|v| !v.is_empty()).ok_or(UndoError::NothingToUndo)?;
    if versions.last().map(|v| v.after) != Some(hash(current)) {
        return Err(UndoError::ChangedSince);
    }
    let v = versions.pop().expect("checked non-empty");
    if versions.is_empty() {
        h.files.remove(&k);
        h.order.retain(|p| p != &k);
    }
    Ok(v.before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_replaces_the_file_and_leaves_no_temp_behind() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        std::fs::write(&p, "old").unwrap();
        write_atomic(&p, b"new content").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new content");
        let leftovers: Vec<_> = std::fs::read_dir(d.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    }

    #[test]
    fn a_new_file_is_created_and_permissions_survive_a_rewrite() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("run.sh");
        write_atomic(&p, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            write_atomic(&p, b"#!/bin/sh\necho hi\n").unwrap();
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o755, "an executable stays executable");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_written_through_not_replaced() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real.txt");
        let link = d.path().join("link.txt");
        std::fs::write(&real, "one").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "two");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link is still a link");
    }

    #[test]
    fn writers_of_one_file_take_turns_and_other_files_do_not_wait() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        assert!(Arc::ptr_eq(&lock_for(&a), &lock_for(&a)));
        assert!(!Arc::ptr_eq(&lock_for(&a), &lock_for(&b)));

        // 8 threads each append a line by read-modify-write; without the lock lines are lost.
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let a = a.clone();
                std::thread::spawn(move || {
                    let l = lock_for(&a);
                    let _g = l.lock().unwrap();
                    let mut text = std::fs::read_to_string(&a).unwrap();
                    text.push_str(&format!("line {i}\n"));
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    write_atomic(&a, text.as_bytes()).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&a).unwrap().lines().count(), 8);
    }

    #[test]
    fn undo_restores_the_previous_content_once_per_edit() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("u.txt");
        record(&p, Some("v1"), "v2");
        record(&p, Some("v2"), "v3");
        assert_eq!(undo(&p, "v3"), Ok(Some("v2".into())));
        assert_eq!(undo(&p, "v2"), Ok(Some("v1".into())));
        assert_eq!(undo(&p, "v1"), Err(UndoError::NothingToUndo));
    }

    #[test]
    fn undo_refuses_to_roll_back_a_change_it_did_not_make() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.txt");
        record(&p, Some("mine-before"), "mine-after");
        // Someone else edited the file since.
        assert_eq!(undo(&p, "theirs"), Err(UndoError::ChangedSince));
        // The version was kept: once the file is as the edit left it, undo works.
        assert_eq!(undo(&p, "mine-after"), Ok(Some("mine-before".into())));
    }

    #[test]
    fn undoing_a_creation_means_removing_the_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("new.txt");
        record(&p, None, "hello");
        assert_eq!(undo(&p, "hello"), Ok(None));
    }

    #[test]
    fn a_huge_file_has_no_undo_rather_than_a_wrong_one() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("big.txt");
        record(&p, Some("small"), "small2");
        record(&p, Some("small2"), &"x".repeat(MAX_VERSION_BYTES + 1));
        assert_eq!(undo(&p, "anything"), Err(UndoError::NothingToUndo));
    }
}

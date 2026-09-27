//! Owner-only file permissions for files holding secrets.
//!
//! `~/.senclaw/` mixes two kinds of file: harmless state (window positions,
//! marketplace cache) and files that are as good as credentials —
//! `config.json` carries every LLM `apiKey`, `senclaw.db` carries channel bot
//! tokens and Space-App access tokens, `project-config.json` carries MCP server
//! env blocks.
//!
//! Only `oauth.json` and `api_token` were ever chmod'ed; everything else was
//! created with the process umask, which on macOS means `0644` —
//! world-readable. Any other account on the machine could read the API keys.
//!
//! This module is the one place that knows the mode, so a new secret-bearing
//! file is one `restrict()` call away from being covered.
//!
//! Scope: this is a boundary against *other OS accounts* and against a file
//! being copied out (backup, sync folder). It is not a boundary against code
//! running as the same user — that needs the vault.

use std::path::Path;

use anyhow::{Context, Result};

/// Restrict `path` to owner read/write (`0600`).
///
/// Missing file is not an error: callers restrict paths that may not exist yet
/// (a DB that has not been created, a config never written), and a `NotFound`
/// there is normal rather than a failure to report.
#[cfg(unix)]
pub fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(_) => std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

/// Windows inherits the user-profile ACL, which is already user-scoped.
#[cfg(not(unix))]
pub fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

/// Restrict `path` and log instead of propagating.
///
/// For call sites on a hot or infallible path (opening the DB, writing config)
/// where a permissions failure must not abort the operation — a readable file
/// is worse than ideal, but a daemon that refuses to boot is worse still.
pub fn restrict_best_effort(path: &Path) {
    if let Err(e) = restrict(path) {
        tracing::warn!("[perms] {e:#}");
    }
}

/// Restrict a SQLite database *and its sidecars*.
///
/// WAL mode keeps recently written pages in `<db>-wal` until checkpoint — on
/// this machine that file was 4 MB — and `<db>-shm` is the shared-memory index.
/// Locking down only the main file leaves fresh rows readable in the sidecar,
/// so all three move together.
pub fn restrict_sqlite(db_path: &Path) {
    restrict_best_effort(db_path);
    for suffix in ["-wal", "-shm"] {
        let mut p = db_path.as_os_str().to_os_string();
        p.push(suffix);
        restrict_best_effort(Path::new(&p));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn restrict_sets_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("secret.json");
        std::fs::write(&f, "{}").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();

        restrict(&f).unwrap();
        assert_eq!(mode_of(&f), 0o600, "got {:o}", mode_of(&f));
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        restrict(&dir.path().join("never-written.json")).unwrap();
    }

    #[test]
    fn sqlite_covers_wal_and_shm() {
        // The sidecars are where uncheckpointed rows live; a 0600 main file
        // with a 0644 -wal still leaks recent writes.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        for name in ["t.db", "t.db-wal", "t.db-shm"] {
            let p = dir.path().join(name);
            std::fs::write(&p, "x").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        restrict_sqlite(&db);
        for name in ["t.db", "t.db-wal", "t.db-shm"] {
            let p = dir.path().join(name);
            assert_eq!(mode_of(&p), 0o600, "{name} got {:o}", mode_of(&p));
        }
    }
}

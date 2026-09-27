//! Filesystem path helpers shared across the daemon.

use std::path::PathBuf;

/// Expand a leading `~` to the user's home directory.
///
/// The New Chat folder picker (Web UI) only captures the *string* the user
/// typed — it deliberately never touches the filesystem itself. That means a
/// `~/projects/foo` path arrives here verbatim, and the backend is responsible
/// for resolving it before opening the workspace. Anything that isn't a bare
/// `~` / `~/...` prefix is returned unchanged.
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

/// Resolve a path a model supplied against the agent's working directory.
///
/// `~` expands first; an absolute path is taken as given; a relative one is
/// joined onto `working_dir`. An empty `working_dir` leaves the path alone —
/// there is nothing to resolve against.
///
/// This is the rule [`crate::lsp::diagnostics_after_write`] already applies to
/// the path it pulls out of a tool result, and `Glob`/`Grep` apply to their
/// default search root. The file tools were the exception: they passed the
/// string straight to `std::fs`, so a relative path resolved against the
/// *daemon process* cwd — for a desktop install, wherever the app was
/// launched, never the chat's project. `Read` and `Edit` then reported "File
/// not found" for a file that was right there, and `Write` silently created it
/// in the wrong directory. Their schemas do ask for an absolute path, but
/// `Glob` *returns* paths relative to the working dir, so a model feeding one
/// back is the ordinary case rather than a mistake.
///
/// It resolves; it does not confine. Nothing here rejects `..`: path
/// confinement would be a new restriction, and a group's `allowed_paths` is
/// not enforced anywhere today.
pub fn resolve_in_workspace(path: &str, working_dir: &str) -> PathBuf {
    let p = expand_tilde(path);
    if p.is_absolute() {
        return p;
    }
    let root = expand_tilde(working_dir);
    // A working dir that is itself relative is no anchor at all, and joining
    // onto it would break the one property the two call layers rely on: the
    // result would still be relative, so a second pass joins again — "src" +
    // "a.txt" gives "src/a.txt", then "src/src/a.txt". `workspace_switch`
    // persists whatever string it is handed (`PathBuf::from`, no
    // absolutisation), so this is reachable rather than hypothetical. Treat it
    // exactly like having no working dir: leave the path alone.
    if !root.is_absolute() {
        return p;
    }
    root.join(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_path_is_taken_as_given() {
        assert_eq!(
            resolve_in_workspace("/etc/hosts", "/work/proj"),
            PathBuf::from("/etc/hosts")
        );
    }

    #[test]
    fn a_relative_path_joins_the_working_dir() {
        // The bug this exists for: without the join this resolved against the
        // daemon process cwd — the app bundle's Resources on a desktop install.
        assert_eq!(
            resolve_in_workspace("src/lib.rs", "/work/proj"),
            PathBuf::from("/work/proj/src/lib.rs")
        );
        assert_eq!(
            resolve_in_workspace("./notes.md", "/work/proj"),
            PathBuf::from("/work/proj/./notes.md")
        );
    }

    #[test]
    fn a_tilde_path_expands_before_anything_else() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(resolve_in_workspace("~/a.txt", "/work"), home.join("a.txt"));
    }

    #[test]
    fn a_relative_working_dir_is_no_anchor_so_resolving_is_idempotent() {
        // Resolution runs at two layers (the run_tools seam and the tool
        // itself). If a relative working dir were joined, the second pass
        // would join it again and the write would land two directories deep.
        let once = resolve_in_workspace("a.txt", "src");
        assert_eq!(once, PathBuf::from("a.txt"));
        let twice = resolve_in_workspace(&once.to_string_lossy(), "src");
        assert_eq!(twice, once, "resolving twice must not move the path");
    }

    #[test]
    fn resolving_an_already_resolved_path_changes_nothing() {
        let once = resolve_in_workspace("a.txt", "/work/proj");
        let twice = resolve_in_workspace(&once.to_string_lossy(), "/work/proj");
        assert_eq!(twice, once);
    }

    #[test]
    fn no_working_dir_leaves_the_path_alone() {
        // A one-shot run may have none (`OneShotOptions::working_dir` defaults
        // to empty); joining onto "" would produce a bare relative path that
        // silently means something else.
        assert_eq!(resolve_in_workspace("a.txt", ""), PathBuf::from("a.txt"));
    }

    #[test]
    fn it_resolves_but_does_not_confine() {
        // Deliberate: nothing here rejects `..`. Confinement would be a new
        // restriction, and a chat's working dir is the user's HOME by default.
        assert_eq!(
            resolve_in_workspace("../sibling/x", "/work/proj"),
            PathBuf::from("/work/proj/../sibling/x")
        );
    }

    #[test]
    fn expands_tilde_slash() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde("~/projects/foo"), home.join("projects/foo"));
        }
    }

    #[test]
    fn expands_bare_tilde() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde("~"), home);
        }
    }

    #[test]
    fn leaves_absolute_unchanged() {
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn does_not_expand_tilde_user() {
        // `~bob/x` is a different user's home — we don't resolve it, leave verbatim.
        assert_eq!(expand_tilde("~bob/x"), PathBuf::from("~bob/x"));
    }
}

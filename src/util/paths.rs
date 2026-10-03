//! Filesystem path helpers shared across the daemon.

use std::path::{Path, PathBuf};

/// Relocates the daemon's state folder — config, database, tokens, logs,
/// runtimes (default `~/.senclaw`).
pub const SENCLAW_HOME_ENV: &str = "SENCLAW_HOME";
/// Relocates the user-data folder — agent profiles, workspaces, wiki,
/// workflows (default `~/senclaw`).
pub const SENCLAW_DATA_HOME_ENV: &str = "SENCLAW_DATA_HOME";

/// The daemon's state folder: `$SENCLAW_HOME`, else `~/.senclaw`.
///
/// Every module derives its default paths from here, so one variable moves a
/// whole daemon — which is what lets an app embed SenClaw as its runtime core
/// without sharing state with the user's own install.
pub fn senclaw_home() -> PathBuf {
    resolve_dirs(env_value(SENCLAW_HOME_ENV), env_value(SENCLAW_DATA_HOME_ENV), &user_home(), &cwd()).0
}

/// The user-data folder: `$SENCLAW_DATA_HOME`; else `$SENCLAW_HOME/data` when
/// `SENCLAW_HOME` moves the state folder; else `~/senclaw`.
pub fn senclaw_data_home() -> PathBuf {
    resolve_dirs(env_value(SENCLAW_HOME_ENV), env_value(SENCLAW_DATA_HOME_ENV), &user_home(), &cwd()).1
}

/// Rewrite a relative or `~`-prefixed `SENCLAW_HOME` / `SENCLAW_DATA_HOME` in
/// this process's environment to the absolute folder it resolved to.
///
/// Children inherit the environment but not the working directory: an MCP
/// server started in a chat's project folder, or a runtime started in its
/// package folder, would otherwise read `./.senclaw-dev` as a different
/// folder than the daemon did. Call once at startup, before anything spawns.
pub fn pin_senclaw_dirs_in_env() {
    let (home, data) = (senclaw_home(), senclaw_data_home());
    for (key, resolved) in [(SENCLAW_HOME_ENV, home), (SENCLAW_DATA_HOME_ENV, data)] {
        if let Some(raw) = env_value(key) {
            if Path::new(&raw) != resolved {
                std::env::set_var(key, resolved);
            }
        }
    }
}

/// `(state folder, user-data folder)` from the two variables' raw values.
///
/// Explicitly naming the default `~/.senclaw` keeps the default `~/senclaw`
/// beside it: setting the variable to where the data already is must not
/// make the user's profiles and workspaces vanish into a new empty folder.
fn resolve_dirs(home_var: Option<String>, data_var: Option<String>, user_home: &Path, cwd: &Path) -> (PathBuf, PathBuf) {
    let default_home = user_home.join(".senclaw");
    let home = home_var.as_deref().map(|v| absolute_dir(v, user_home, cwd));
    let data = match (data_var.as_deref(), &home) {
        (Some(v), _) => absolute_dir(v, user_home, cwd),
        (None, Some(h)) if *h != default_home => h.join("data"),
        (None, _) => user_home.join("senclaw"),
    };
    (home.unwrap_or(default_home), data)
}

/// `~` expanded, then a relative path anchored at `cwd`.
fn absolute_dir(raw: &str, user_home: &Path, cwd: &Path) -> PathBuf {
    let p = match raw.strip_prefix("~/") {
        Some(rest) => user_home.join(rest),
        None if raw == "~" => user_home.to_path_buf(),
        None => PathBuf::from(raw),
    };
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

/// A variable's value, trimmed; unset and blank both read as `None`.
fn env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn user_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

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

    fn dirs_for(home_var: Option<&str>, data_var: Option<&str>) -> (PathBuf, PathBuf) {
        resolve_dirs(
            home_var.map(str::to_string),
            data_var.map(str::to_string),
            Path::new("/Users/u"),
            Path::new("/work/app"),
        )
    }

    #[test]
    fn unset_variables_keep_the_two_home_folders() {
        assert_eq!(
            dirs_for(None, None),
            (PathBuf::from("/Users/u/.senclaw"), PathBuf::from("/Users/u/senclaw"))
        );
    }

    #[test]
    fn senclaw_home_alone_moves_the_user_data_with_it() {
        // One variable must be enough for an embedding app: leaving the data
        // at `~/senclaw` would share profiles and workspaces with the user's
        // own daemon.
        assert_eq!(
            dirs_for(Some("/apps/news/.senclaw"), None),
            (PathBuf::from("/apps/news/.senclaw"), PathBuf::from("/apps/news/.senclaw/data"))
        );
    }

    #[test]
    fn naming_the_default_home_keeps_the_default_data_folder() {
        assert_eq!(dirs_for(Some("~/.senclaw"), None).1, PathBuf::from("/Users/u/senclaw"));
        assert_eq!(dirs_for(Some("/Users/u/.senclaw/"), None).1, PathBuf::from("/Users/u/senclaw"));
    }

    #[test]
    fn data_home_is_independent_when_set() {
        assert_eq!(
            dirs_for(Some("/apps/news/state"), Some("/apps/news/data")),
            (PathBuf::from("/apps/news/state"), PathBuf::from("/apps/news/data"))
        );
        assert_eq!(dirs_for(None, Some("/d")), (PathBuf::from("/Users/u/.senclaw"), PathBuf::from("/d")));
    }

    #[test]
    fn relative_and_tilde_values_resolve_to_absolute_folders() {
        assert_eq!(
            dirs_for(Some(".senclaw-dev"), Some("~/dev-data")),
            (PathBuf::from("/work/app/.senclaw-dev"), PathBuf::from("/Users/u/dev-data"))
        );
    }

    #[test]
    fn does_not_expand_tilde_user() {
        // `~bob/x` is a different user's home — we don't resolve it, leave verbatim.
        assert_eq!(expand_tilde("~bob/x"), PathBuf::from("~bob/x"));
    }
}

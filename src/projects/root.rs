//! Which folders may be a project's root.
//!
//! A root becomes the working directory of every session in the project and
//! the only folder those sessions may switch within, so the folders that must
//! never be one are the broad ones (`/`, the home folder and anything holding
//! it) and the ones SenClaw keeps its own state in. The check runs when a
//! project is created or its root edited, and again each time a session's core
//! is built — a folder can be renamed, deleted or replaced by a symlink after
//! the project was saved.
//!
//! "Another project already has this root" is not decided here: it needs the
//! database (`Db::find_project_by_root`).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Config;

/// One reason a folder cannot be a root. `code` is for clients to branch on;
/// `message` is for a person who has no client to translate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootProblem {
    pub code: &'static str,
    pub message: String,
}

impl RootProblem {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The folders a root must stay away from.
#[derive(Debug, Clone)]
pub struct RootRules {
    pub user_home: PathBuf,
    /// `~/.senclaw`: the token, the databases and every chat.
    pub senclaw_home: PathBuf,
    /// `<data home>/profiles`: each agent's persona and memory.
    pub profiles_dir: PathBuf,
    /// `<data home>/projects`: the context files this module writes.
    pub projects_dir: PathBuf,
}

impl RootRules {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            user_home: dirs::home_dir().unwrap_or_default(),
            senclaw_home: crate::util::paths::senclaw_home(),
            profiles_dir: cfg.paths.profiles_dir.clone(),
            projects_dir: cfg.paths.projects_dir.clone(),
        }
    }
}

/// What looking at a candidate root found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootCheck {
    /// The absolute path with symlinks and `..` resolved; `None` while the
    /// folder could not be resolved at all.
    pub canonical: Option<PathBuf>,
    pub problems: Vec<RootProblem>,
    /// The folder already has a `CLAUDE.md`/`SENCLAW.md`, which the agent
    /// reads on its own — the project's context adds to it.
    pub has_claude_md: bool,
    pub is_git: bool,
}

impl RootCheck {
    pub fn ok(&self) -> bool {
        self.problems.is_empty() && self.canonical.is_some()
    }
}

/// Look at `raw` (what the person typed; a leading `~` is expanded).
pub fn check_root(raw: &str, rules: &RootRules) -> RootCheck {
    let mut check = RootCheck::default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        check
            .problems
            .push(RootProblem::new("empty", "The root folder is empty."));
        return check;
    }
    let expanded = crate::util::paths::expand_tilde(trimmed);
    if !expanded.is_absolute() {
        check.problems.push(RootProblem::new(
            "not_absolute",
            format!("\"{trimmed}\" is not an absolute path."),
        ));
        return check;
    }
    let canonical = match std::fs::canonicalize(&expanded) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            check.problems.push(RootProblem::new(
                "not_found",
                format!("{} does not exist.", expanded.display()),
            ));
            return check;
        }
        Err(e) => {
            check.problems.push(RootProblem::new(
                "unreadable",
                format!("{} cannot be resolved: {e}.", expanded.display()),
            ));
            return check;
        }
    };
    if !canonical.is_dir() {
        check.problems.push(RootProblem::new(
            "not_a_directory",
            format!("{} is not a folder.", canonical.display()),
        ));
        check.canonical = Some(canonical);
        return check;
    }
    if let Err(e) = std::fs::read_dir(&canonical) {
        check.problems.push(RootProblem::new(
            "unreadable",
            format!("{} cannot be read: {e}.", canonical.display()),
        ));
    }
    check.problems.extend(forbidden(&canonical, rules));
    check.has_claude_md =
        canonical.join("CLAUDE.md").is_file() || canonical.join("SENCLAW.md").is_file();
    check.is_git = canonical.join(".git").exists();
    check.canonical = Some(canonical);
    check
}

/// A stored root, checked again before it is used. `Ok` is the canonical path.
pub fn recheck_root(root: &str, rules: &RootRules) -> Result<PathBuf, RootProblem> {
    let mut check = check_root(root, rules);
    if check.problems.is_empty() {
        if let Some(p) = check.canonical.take() {
            return Ok(p);
        }
    }
    Err(check
        .problems
        .into_iter()
        .next()
        .unwrap_or_else(|| RootProblem::new("unreadable", format!("{root} cannot be used."))))
}

fn forbidden(canonical: &Path, rules: &RootRules) -> Vec<RootProblem> {
    let mut out = Vec::new();
    if canonical.parent().is_none() {
        out.push(RootProblem::new(
            "filesystem_root",
            "The top of the filesystem cannot be a project root.",
        ));
        return out;
    }
    let home = canonical_lenient(&rules.user_home);
    if !rules.user_home.as_os_str().is_empty() {
        if canonical == home {
            out.push(RootProblem::new(
                "home_dir",
                "The home folder is too broad to be a project root; pick a folder inside it.",
            ));
        } else if home.starts_with(canonical) {
            out.push(RootProblem::new(
                "contains_home",
                format!(
                    "{} contains the home folder, which is too broad to be a project root.",
                    canonical.display()
                ),
            ));
        }
    }
    let state = canonical_lenient(&rules.senclaw_home);
    if canonical.starts_with(&state) || state.starts_with(canonical) {
        out.push(RootProblem::new(
            "senclaw_state",
            format!(
                "{} holds SenClaw's own state (tokens, databases, chats); it cannot be a project root.",
                state.display()
            ),
        ));
        return out;
    }
    if canonical.starts_with(canonical_lenient(&rules.profiles_dir)) {
        out.push(RootProblem::new(
            "senclaw_profiles",
            "The agent profiles folder cannot be a project root.",
        ));
    } else if canonical.starts_with(canonical_lenient(&rules.projects_dir)) {
        out.push(RootProblem::new(
            "senclaw_projects",
            "The folder SenClaw keeps project contexts in cannot be a project root.",
        ));
    }
    out
}

/// `p` with symlinks resolved as far as it exists, the missing tail appended
/// as written. A rule folder that has not been created yet (the first project
/// has no `projects/` yet) must still compare equal to its canonical spelling:
/// on macOS `/var/…` and `/private/var/…` are the same folder.
fn canonical_lenient(p: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(c) = std::fs::canonicalize(&cur) {
            let mut out = c;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake user home with SenClaw's folders inside it, and rules to match.
    struct World {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        rules: RootRules,
    }

    fn world() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(tmp.path()).unwrap().join("home");
        let state = home.join(".senclaw");
        let data = home.join("senclaw");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(data.join("profiles")).unwrap();
        // `projects/` is deliberately not created: the first project is made
        // before any context file exists.
        let rules = RootRules {
            user_home: home.clone(),
            senclaw_home: state,
            profiles_dir: data.join("profiles"),
            projects_dir: data.join("projects"),
        };
        World {
            _tmp: tmp,
            home,
            rules,
        }
    }

    fn codes(c: &RootCheck) -> Vec<&'static str> {
        c.problems.iter().map(|p| p.code).collect()
    }

    #[test]
    fn an_ordinary_folder_is_accepted_and_reported_canonical() {
        let w = world();
        let repo = w.home.join("code").join("app");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join("CLAUDE.md"), "x").unwrap();
        let c = check_root(&format!("{}/", repo.display()), &w.rules);
        assert!(c.ok(), "{:?}", c.problems);
        assert_eq!(c.canonical.as_deref(), Some(repo.as_path()));
        assert!(c.is_git && c.has_claude_md);
    }

    #[cfg(unix)]
    #[test]
    fn dotdot_and_symlinks_are_resolved_before_anything_is_decided() {
        let w = world();
        let real = w.home.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = w.home.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        for spelling in [
            link.display().to_string(),
            format!("{}/real/../real", w.home.display()),
        ] {
            let c = check_root(&spelling, &w.rules);
            assert_eq!(c.canonical.as_deref(), Some(real.as_path()), "{spelling}");
        }
        // A link into the state folder is the state folder.
        let sneaky = w.home.join("sneaky");
        std::os::unix::fs::symlink(&w.rules.senclaw_home, &sneaky).unwrap();
        assert_eq!(
            codes(&check_root(&sneaky.display().to_string(), &w.rules)),
            ["senclaw_state"]
        );
    }

    #[test]
    fn the_top_of_the_filesystem_and_the_home_folder_are_refused() {
        let w = world();
        assert_eq!(codes(&check_root("/", &w.rules)), ["filesystem_root"]);
        // The home folder holds SenClaw's state too, so it earns both reasons.
        let home = check_root(&w.home.display().to_string(), &w.rules);
        assert_eq!(codes(&home), ["home_dir", "senclaw_state"]);
        assert!(!home.ok());
        // Anything that holds the home folder is at least as broad.
        let parent = check_root(&w.home.parent().unwrap().display().to_string(), &w.rules);
        assert_eq!(codes(&parent), ["contains_home", "senclaw_state"]);
    }

    #[test]
    fn senclaws_own_folders_are_refused_but_the_rest_of_the_data_home_is_not() {
        let w = world();
        let state = &w.rules.senclaw_home;
        std::fs::create_dir_all(state.join("sandbox")).unwrap();
        for inside in [state.clone(), state.join("sandbox")] {
            assert_eq!(
                codes(&check_root(&inside.display().to_string(), &w.rules)),
                ["senclaw_state"]
            );
        }
        let profiles = &w.rules.profiles_dir;
        std::fs::create_dir_all(profiles.join("main")).unwrap();
        for inside in [profiles.clone(), profiles.join("main")] {
            assert_eq!(
                codes(&check_root(&inside.display().to_string(), &w.rules)),
                ["senclaw_profiles"]
            );
        }
        let projects = &w.rules.projects_dir;
        std::fs::create_dir_all(projects.join("prj_00000000")).unwrap();
        for inside in [projects.clone(), projects.join("prj_00000000")] {
            assert_eq!(
                codes(&check_root(&inside.display().to_string(), &w.rules)),
                ["senclaw_projects"]
            );
        }
        // `~/senclaw/workspace/...` is where chats work by default.
        let workspace = w.home.join("senclaw").join("workspace").join("main");
        std::fs::create_dir_all(&workspace).unwrap();
        assert!(check_root(&workspace.display().to_string(), &w.rules).ok());
    }

    #[test]
    fn a_state_folder_that_does_not_exist_yet_is_still_found_inside_its_parent() {
        // The rule is written with the spelling the OS handed out (on macOS
        // the temp folder is `/var/…`, really `/private/var/…`) and its last
        // component has not been created. A root that is its existing parent
        // must still be recognised as holding it.
        let tmp = tempfile::tempdir().unwrap();
        let rules = RootRules {
            user_home: PathBuf::from("/nonexistent-home-for-test"),
            senclaw_home: tmp.path().join("not-yet").join(".senclaw"),
            profiles_dir: tmp.path().join("data").join("profiles"),
            projects_dir: tmp.path().join("data").join("projects"),
        };
        let c = check_root(&tmp.path().display().to_string(), &rules);
        assert_eq!(codes(&c), ["senclaw_state"]);
    }

    #[test]
    fn what_is_not_a_usable_folder_is_said_plainly() {
        let w = world();
        let file = w.home.join("notes.txt");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(codes(&check_root("", &w.rules)), ["empty"]);
        assert_eq!(codes(&check_root("   ", &w.rules)), ["empty"]);
        assert_eq!(
            codes(&check_root("relative/dir", &w.rules)),
            ["not_absolute"]
        );
        assert_eq!(
            codes(&check_root(
                &w.home.join("missing").display().to_string(),
                &w.rules
            )),
            ["not_found"]
        );
        let c = check_root(&file.display().to_string(), &w.rules);
        assert_eq!(codes(&c), ["not_a_directory"]);
        assert!(!c.ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_stored_root_is_judged_again_when_it_is_used() {
        let w = world();
        let repo = w.home.join("app");
        std::fs::create_dir_all(&repo).unwrap();
        let stored = repo.display().to_string();
        assert_eq!(recheck_root(&stored, &w.rules).unwrap(), repo);
        std::fs::remove_dir_all(&repo).unwrap();
        assert_eq!(
            recheck_root(&stored, &w.rules).unwrap_err().code,
            "not_found"
        );
        // Replaced by a link to a folder that is not allowed.
        std::os::unix::fs::symlink(&w.rules.senclaw_home, &repo).unwrap();
        assert_eq!(
            recheck_root(&stored, &w.rules).unwrap_err().code,
            "senclaw_state"
        );
    }
}

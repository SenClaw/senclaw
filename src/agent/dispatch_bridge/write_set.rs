//! Write-set declarations and the disjointness test the scheduler gates on.
//!
//! A task declares the paths it will write as globs. Two tasks may run at the
//! same time when their declarations cannot name the same file.
//!
//! The test compares **declaration against declaration**, not glob against a
//! real path, so it deliberately answers a weaker question than "will these two
//! actually collide": it answers "could they". Deciding glob-vs-glob
//! intersection exactly is not worth its complexity here, so each pattern is
//! reduced to the literal path components before its first wildcard and two
//! patterns are called disjoint only when neither prefix contains the other.
//! Every approximation therefore errs toward *serializing* work that might have
//! been safe to parallelize, never toward letting a collision through.

/// Characters that end a pattern's literal prefix.
const WILDCARD: [char; 4] = ['*', '?', '[', '{'];

/// Strip `workspace` and any leading `./` or `/` so an absolute declaration and
/// a workspace-relative one compare as the same path.
fn normalize(pattern: &str, workspace: &str) -> String {
    let p = pattern.trim();
    let ws = workspace.trim().trim_end_matches('/');
    let p = if !ws.is_empty() {
        p.strip_prefix(ws).unwrap_or(p)
    } else {
        p
    };
    p.trim_start_matches('/')
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// The path components before the pattern's first wildcard.
///
/// `src/agent/**/*.rs` → `["src", "agent"]`, `src/a.rs` → `["src", "a.rs"]`,
/// `**/*.rs` → `[]` (matches anywhere, so it overlaps everything).
fn literal_prefix(pattern: &str, workspace: &str) -> Vec<String> {
    normalize(pattern, workspace)
        .split('/')
        .take_while(|seg| !seg.contains(WILDCARD))
        .filter(|seg| !seg.is_empty() && *seg != ".")
        .map(|seg| seg.to_string())
        .collect()
}

/// Could these two patterns ever name the same file?
///
/// Compared component-wise on purpose: as raw strings `src/foo` is a prefix of
/// `src/foobar.rs`, which would serialize two unrelated files forever.
fn patterns_may_overlap(a: &str, b: &str, workspace: &str) -> bool {
    let (pa, pb) = (literal_prefix(a, workspace), literal_prefix(b, workspace));
    pa.iter().zip(pb.iter()).all(|(x, y)| x == y)
}

/// Could any pattern in `a` name a file any pattern in `b` also names?
///
/// An **empty** set is "not declared", not "writes nothing" — the caller decides
/// what that means, because the two readings differ: an undeclared task is
/// unconstrained (today's behaviour) while a task that writes nothing should be
/// `TaskIo::ReadOnly` and say so.
pub fn sets_may_overlap(a: &[String], b: &[String], workspace: &str) -> bool {
    a.iter()
        .any(|pa| b.iter().any(|pb| patterns_may_overlap(pa, pb, workspace)))
}

/// Do two workspace paths refer to the same directory? Trailing separators only.
pub fn same_workspace(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sibling_directories_are_disjoint() {
        assert!(!sets_may_overlap(
            &set(&["src/agent/**"]),
            &set(&["src/mcp/**"]),
            ""
        ));
    }

    #[test]
    fn a_broader_pattern_overlaps_one_nested_inside_it() {
        assert!(sets_may_overlap(
            &set(&["src/**/*.rs"]),
            &set(&["src/agent/foo.rs"]),
            ""
        ));
    }

    #[test]
    fn distinct_files_in_one_directory_are_disjoint() {
        assert!(!sets_may_overlap(
            &set(&["src/a.rs"]),
            &set(&["src/b.rs"]),
            ""
        ));
    }

    #[test]
    fn a_shared_string_prefix_is_not_a_shared_path() {
        // `src/foo` string-prefixes `src/foobar.rs`; as paths they are unrelated
        // and must be allowed to run together.
        assert!(!sets_may_overlap(
            &set(&["src/foo/**"]),
            &set(&["src/foobar.rs"]),
            ""
        ));
    }

    #[test]
    fn a_leading_wildcard_overlaps_everything() {
        assert!(sets_may_overlap(
            &set(&["**/*.rs"]),
            &set(&["src/a.rs"]),
            ""
        ));
    }

    #[test]
    fn the_same_file_overlaps_itself() {
        assert!(sets_may_overlap(
            &set(&["src/a.rs"]),
            &set(&["src/a.rs"]),
            ""
        ));
    }

    #[test]
    fn overlap_is_checked_across_every_pair_not_just_the_first() {
        assert!(sets_may_overlap(
            &set(&["docs/a.md", "src/agent/x.rs"]),
            &set(&["web/b.ts", "src/agent/x.rs"]),
            ""
        ));
    }

    #[test]
    fn absolute_and_relative_declarations_compare_equal() {
        let ws = "/Users/x/repo";
        assert!(sets_may_overlap(
            &set(&["/Users/x/repo/src/a.rs"]),
            &set(&["src/a.rs"]),
            ws
        ));
        assert!(!sets_may_overlap(
            &set(&["/Users/x/repo/src/a.rs"]),
            &set(&["src/b.rs"]),
            ws
        ));
    }

    #[test]
    fn a_dot_slash_prefix_does_not_make_paths_differ() {
        assert!(sets_may_overlap(
            &set(&["./src/a.rs"]),
            &set(&["src/a.rs"]),
            ""
        ));
    }

    #[test]
    fn an_empty_set_never_reports_overlap_on_its_own() {
        // Callers give the empty set its meaning; the pure test just has no
        // pattern that could match anything.
        assert!(!sets_may_overlap(&[], &set(&["src/a.rs"]), ""));
        assert!(!sets_may_overlap(&set(&["src/a.rs"]), &[], ""));
    }

    #[test]
    fn workspace_paths_compare_ignoring_a_trailing_separator() {
        assert!(same_workspace("/w/repo", "/w/repo/"));
        assert!(!same_workspace("/w/repo", "/w/other"));
    }
}

//! Bringing a git source down and refreshing it.
//!
//! Reuses [`crate::marketplace::git_sync::clone_or_pull`] rather than shelling
//! out to `git` — same clone/fetch/checkout behaviour the marketplace already
//! relies on, one implementation to keep working.
//!
//! ## Why the ref matters more here than for a marketplace clone
//!
//! A pattern is placed in the **system prompt** position of a real LLM call.
//! Following a moving branch therefore means an upstream commit can silently
//! rewrite instructions the agent then obeys — the plain prompt-injection
//! shape, with the repo owner as the injector. Nothing here can decide the
//! trust question for the user, so the sync does two things instead: it
//! records what it fetched, and [`SourceSyncOutcome::pinned`] tells the UI
//! whether the source is following a branch or sitting on a fixed tag, so
//! "unpinned" can be shown as the risk it is.

use std::path::Path;

use serde::Serialize;

use super::store::{PatternStore, StoreError};
use super::SourceKind;

/// What one sync did.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSyncOutcome {
    pub source: String,
    /// Patterns visible in the source after the sync.
    pub patterns: usize,
    /// Strategy files copied into the shared strategies dir this run.
    pub strategies_imported: Vec<String>,
    /// False when `git_ref` looks like a branch rather than a tag/commit.
    pub pinned: bool,
    pub synced_at: String,
}

/// Heuristic: does this ref look like a fixed point in history?
///
/// Deliberately conservative — anything that is not obviously a version tag or
/// a full-length hex sha is reported as unpinned, so the warning errs toward
/// showing up rather than staying quiet.
pub fn looks_pinned(git_ref: &str) -> bool {
    let r = git_ref.trim();
    if r.len() == 40 && r.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    let stripped = r.strip_prefix('v').unwrap_or(r);
    !stripped.is_empty()
        && stripped
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
        && stripped.chars().any(|c| c.is_ascii_digit())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Clone or pull `src` and refresh what the registry can see.
///
/// On success the source's `lastSyncedAt` is stamped and `lastError` cleared;
/// on failure the error is stored on the source and returned. Either way the
/// ledger is written, so a source that has been failing for a week says so
/// instead of just looking empty.
pub fn sync_source(
    store: &PatternStore,
    source_id: &str,
) -> Result<SourceSyncOutcome, StoreError> {
    let mut src = store.source(source_id)?;

    if src.kind != SourceKind::Git {
        // A local source has nothing to fetch; report its size rather than
        // failing, so the UI can offer one "Sync" button for every row.
        return Ok(SourceSyncOutcome {
            patterns: store.names_in(&src).len(),
            source: src.id,
            strategies_imported: Vec::new(),
            pinned: true,
            synced_at: now_rfc3339(),
        });
    }

    let url = src
        .url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| StoreError::Io(format!("source \"{source_id}\" has no git url")))?;

    let checkout = store.checkout_dir(&src.id);
    if let Some(parent) = checkout.parent() {
        std::fs::create_dir_all(parent)?;
    }

    match fetch(&url, &src.git_ref, &checkout) {
        Ok(()) => {
            src.last_error = None;
            src.last_synced_at = Some(now_rfc3339());
        }
        Err(e) => {
            let msg = e.to_string();
            src.last_error = Some(msg.clone());
            store.upsert_source(src)?;
            return Err(StoreError::Io(msg));
        }
    }

    // Strategies ship inside the same repo for Fabric; copy them out into the
    // shared dir so they apply to every pattern, not just this source's.
    let strategies_imported = match src.strategies_subdir.clone() {
        Some(sub) if !sub.trim().is_empty() => {
            let from = join_inside(&checkout, &sub);
            store.import_strategies(&from).unwrap_or_default()
        }
        _ => Vec::new(),
    };

    let outcome = SourceSyncOutcome {
        patterns: store.names_in(&src).len(),
        strategies_imported,
        pinned: looks_pinned(&src.git_ref),
        synced_at: src.last_synced_at.clone().unwrap_or_else(now_rfc3339),
        source: src.id.clone(),
    };
    store.upsert_source(src)?;
    Ok(outcome)
}

/// How much history a pattern checkout keeps. One commit: nothing reads a
/// pattern's git log, and a full `danielmiessler/fabric` clone measured ~400
/// seconds against roughly a tenth of that shallow.
const CLONE_DEPTH: i32 = 1;

/// Bring the checkout up to date, preferring a shallow clone.
///
/// Falls back to a full clone when the shallow one fails, because two cases
/// legitimately need history: a `git_ref` that is a raw sha (libgit2 cannot
/// `branch()` to one) and a server with shallow fetches disabled. Re-cloning
/// after a failure costs one wasted attempt on a source that was going to be
/// slow either way.
///
/// A **refresh** of an existing checkout is also a re-clone rather than a
/// pull: a depth-1 repository has no history to fast-forward through, and
/// `git_sync::pull_existing` assumes a normal clone.
fn fetch(url: &str, git_ref: &str, checkout: &Path) -> anyhow::Result<()> {
    match crate::marketplace::git_sync::clone_shallow(url, git_ref, checkout, CLONE_DEPTH) {
        Ok(()) => Ok(()),
        Err(shallow_err) => {
            tracing::warn!(
                "[patterns] shallow clone of {url}@{git_ref} failed ({shallow_err}); \
                 retrying with full history"
            );
            crate::marketplace::git_sync::clone_or_pull(url, git_ref, checkout)
        }
    }
}

/// Join a relative sub-path under `base`, dropping any component that would
/// climb out of it.
fn join_inside(base: &Path, sub: &str) -> std::path::PathBuf {
    sub.split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .fold(base.to_path_buf(), |acc, seg| acc.join(seg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::PatternSource;

    #[test]
    fn tags_and_shas_read_as_pinned_branches_do_not() {
        assert!(looks_pinned("v1.4.0"));
        assert!(looks_pinned("1.4.0"));
        assert!(looks_pinned("0123456789abcdef0123456789abcdef01234567"));
        assert!(!looks_pinned("main"));
        assert!(!looks_pinned("master"));
        assert!(!looks_pinned("release/next"));
        assert!(!looks_pinned(""));
    }

    #[test]
    fn join_inside_never_climbs_out() {
        let base = Path::new("/tmp/checkout");
        assert_eq!(
            join_inside(base, "data/strategies"),
            Path::new("/tmp/checkout/data/strategies")
        );
        assert!(join_inside(base, "../../etc").starts_with(base));
    }

    #[test]
    fn a_local_source_syncs_to_a_no_op_report() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PatternStore::new(tmp.path());
        let user = PatternSource::user();
        store.write(&user, "p", "# H\n\nx", None, false).unwrap();

        let out = sync_source(&store, "user").unwrap();
        assert_eq!(out.patterns, 1);
        assert!(out.pinned);
    }

    #[test]
    fn a_git_source_with_no_url_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PatternStore::new(tmp.path());
        store
            .upsert_source(PatternSource {
                id: "broken".into(),
                kind: SourceKind::Git,
                url: None,
                ..PatternSource::for_kit("broken")
            })
            .unwrap();
        assert!(sync_source(&store, "broken").is_err());
    }

    /// The one test that actually reaches the network, so it is `#[ignore]`d:
    /// `cargo test --lib real_fabric -- --ignored --nocapture`.
    ///
    /// Everything else about the git path is unit-tested offline, but the
    /// shape of the *real* Fabric repo — that `data/patterns` is where the
    /// folders are, that each holds a `system.md`, that `data/strategies` is
    /// parseable — is an assumption about someone else's repository, and the
    /// only way to know it still holds is to fetch it.
    #[test]
    #[ignore = "clones danielmiessler/fabric over the network"]
    fn real_fabric_clone_imports_patterns_and_strategies() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PatternStore::new(tmp.path());
        store
            .upsert_source(PatternSource {
                id: "fabric".into(),
                name: "Fabric".into(),
                kind: SourceKind::Git,
                url: Some("https://github.com/danielmiessler/fabric".into()),
                git_ref: "main".into(),
                subdir: "data/patterns".into(),
                strategies_subdir: Some("data/strategies".into()),
                ..PatternSource::for_kit("fabric")
            })
            .unwrap();

        let out = sync_source(&store, "fabric").expect("clone failed");
        eprintln!(
            "[real fabric] {} patterns, strategies: {:?}, pinned={}",
            out.patterns, out.strategies_imported, out.pinned
        );

        assert!(out.patterns > 150, "expected a full library, got {}", out.patterns);
        assert!(!out.pinned, "`main` must report as unpinned");
        assert!(out.strategies_imported.iter().any(|s| s == "cot"));

        // A pattern everyone knows, resolving through the registry and
        // rendering the way the module docs claim it does.
        let reg = crate::patterns::PatternRegistry::new(&store);
        let (_, files) = reg.resolve("summarize").expect("no `summarize` pattern");
        assert!(files.system.contains("IDENTITY"));

        let rendered = crate::patterns::render_pattern(&crate::patterns::RenderRequest {
            system: &files.system,
            input: "xin chào",
            language: Some("auto"),
            ..Default::default()
        });
        // No `{{input}}` in Fabric's `summarize`, so the text is the user
        // message and the language rule lands last.
        assert_eq!(rendered.user, "xin chào");
        assert!(rendered.system.trim_end().ends_with("in English."));
    }

    #[test]
    fn a_failed_fetch_is_recorded_on_the_source() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PatternStore::new(tmp.path());
        store
            .upsert_source(PatternSource {
                id: "nope".into(),
                kind: SourceKind::Git,
                url: Some("file:///definitely/not/a/repo".into()),
                ..PatternSource::for_kit("nope")
            })
            .unwrap();

        assert!(sync_source(&store, "nope").is_err());
        // The point of the ledger write: the next list call can say *why* the
        // source is empty instead of just showing nothing.
        assert!(store.source("nope").unwrap().last_error.is_some());
    }
}

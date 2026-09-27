//! Compile-time build identity, baked in by `build.rs`.
//!
//! One place to answer "which build is this?": the Cargo version is the
//! release identity (== git tag, see docs/desktop-app-auto-update.md), and
//! the git hash pins the exact commit — the two can disagree on a dev build,
//! which is precisely when you want to see both.

/// Cargo package version — the release identity.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `git describe --tags --always --dirty=+` at build time; "unknown" when the
/// build had no git context (source tarball).
pub const GIT_DESCRIBE: &str = env!("SENCLAW_GIT_DESCRIBE");

/// Short commit hash at build time.
pub const GIT_HASH: &str = env!("SENCLAW_GIT_HASH");

/// Unix seconds when the build script ran.
pub const BUILD_EPOCH: &str = env!("SENCLAW_BUILD_EPOCH");

/// Target triple the binary was compiled for.
pub const BUILD_TARGET: &str = env!("SENCLAW_BUILD_TARGET");

/// What clap prints for `--version`: version plus the commit, one line.
pub const CLAP_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("SENCLAW_GIT_HASH"),
    ")"
);

/// Build time as UTC, or the raw epoch when it does not parse.
pub fn build_time_utc() -> String {
    BUILD_EPOCH
        .parse::<i64>()
        .ok()
        .and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s, 0))
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| BUILD_EPOCH.to_string())
}

/// The full block `senclaw version` prints.
pub fn pretty() -> String {
    format!(
        "senclaw {VERSION}\n\
         commit:  {GIT_HASH} ({GIT_DESCRIBE})\n\
         built:   {}\n\
         target:  {BUILD_TARGET}",
        build_time_utc()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_baked_in_and_non_empty() {
        assert!(!VERSION.is_empty());
        assert!(!GIT_DESCRIBE.is_empty());
        assert!(!GIT_HASH.is_empty());
        assert!(!BUILD_TARGET.is_empty());
        assert!(BUILD_EPOCH.parse::<i64>().is_ok(), "epoch must be numeric");
    }

    #[test]
    fn clap_version_couples_release_version_with_commit() {
        assert!(CLAP_VERSION.starts_with(VERSION));
        assert!(CLAP_VERSION.contains(GIT_HASH));
    }

    #[test]
    fn pretty_reports_every_field() {
        let p = pretty();
        assert!(p.contains(VERSION));
        assert!(p.contains(GIT_HASH));
        assert!(p.contains(BUILD_TARGET));
        assert!(p.contains("built:"));
        // A real build stamps a real clock — not the epoch-0 fallback.
        assert!(!p.contains("1970-01-01"), "build time fell back to 0: {p}");
    }
}

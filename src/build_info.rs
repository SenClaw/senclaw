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

/// How this binary was installed, which decides what may update it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallKind {
    /// Inside the desktop app's bundle: only the app's own updater replaces
    /// it, so daemon and app stay one release.
    DesktopBundle,
    /// A debug build, or one run straight out of a Cargo `target/` dir.
    Dev,
    /// A release binary on its own — what `senclaw update` swaps.
    Standalone,
}

/// Classify the binary at `exe` (normally `current_exe()`, canonicalized).
pub fn install_kind(exe: &std::path::Path) -> InstallKind {
    if desktop_bundle_of(exe).is_some() {
        InstallKind::DesktopBundle
    } else if cfg!(debug_assertions) || in_cargo_target_dir(exe) {
        InstallKind::Dev
    } else {
        InstallKind::Standalone
    }
}

/// The desktop app bundle `exe` belongs to, if any: a macOS `.app` around it,
/// or (Windows/Linux) the app's own executable beside it.
pub fn desktop_bundle_of(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    if let Some(app) = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")) {
        return Some(app.to_path_buf());
    }
    let dir = exe.parent()?;
    ["senclaw_desktop", "senclaw_desktop.exe"]
        .iter()
        .any(|name| dir.join(name).is_file())
        .then(|| dir.to_path_buf())
}

/// `…/<workspace>/target/<profile>/senclaw`, with the workspace's Cargo.toml
/// beside that `target` dir.
fn in_cargo_target_dir(exe: &std::path::Path) -> bool {
    exe.ancestors()
        .filter(|p| p.file_name().is_some_and(|n| n == "target"))
        .any(|target| target.parent().is_some_and(|ws| ws.join("Cargo.toml").is_file()))
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

    #[test]
    fn a_daemon_inside_the_desktop_app_is_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("SenClaw Desktop.app");
        let mac = app.join("Contents").join("Resources").join("senclaw");
        assert_eq!(desktop_bundle_of(&mac), Some(app));
        assert_eq!(install_kind(&mac), InstallKind::DesktopBundle);

        let win = dir.path().join("SenClaw");
        std::fs::create_dir_all(&win).unwrap();
        std::fs::write(win.join("senclaw_desktop.exe"), b"").unwrap();
        assert_eq!(desktop_bundle_of(&win.join("senclaw.exe")), Some(win.clone()));

        let cli = dir.path().join(".senclaw").join("bin").join("senclaw");
        assert_eq!(desktop_bundle_of(&cli), None);
    }

    #[test]
    fn a_binary_in_a_cargo_target_dir_is_a_dev_build() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("senclaw");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("Cargo.toml"), b"").unwrap();
        assert!(in_cargo_target_dir(&ws.join("target").join("release").join("senclaw")));
        // A `target` dir that is no Cargo workspace's says nothing.
        assert!(!in_cargo_target_dir(&dir.path().join("target").join("senclaw")));
        assert!(!in_cargo_target_dir(&dir.path().join(".local").join("bin").join("senclaw")));
    }

    #[test]
    fn install_kinds_serialize_as_the_api_spells_them() {
        let names: Vec<String> = [InstallKind::DesktopBundle, InstallKind::Dev, InstallKind::Standalone]
            .iter()
            .map(|k| serde_json::to_value(k).unwrap().as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["desktop-bundle", "dev", "standalone"]);
    }
}

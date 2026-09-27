//! How much of the real disk a sandbox may **read**.
//!
//! Writing was always confined to the sandbox's own directory. Reading is the
//! axis this module adds, because "cannot change my machine" and "cannot see my
//! files" are different promises and users want to pick.
//!
//! ```text
//! strict     only the sandbox directory + its mounts (+ system libraries)
//! allowlist  strict, plus the folders declared up front in settings
//! open       the whole disk is readable, except the secret folders
//! ```
//!
//! ## Why the system roots are always readable
//!
//! Even `strict` allows `/usr`, `/System`, `/opt/homebrew` and friends. That is
//! not a leak the mode forgot to close — it is what an interpreter *is*. Python
//! lives there, its standard library lives there, the dynamic linker's cache
//! lives there. A read jail that excludes them does not isolate Python; it
//! prevents Python from starting. What `strict` actually removes is reach into
//! the user's own data: documents, projects, other apps' files, the rest of
//! `$HOME`.
//!
//! Docker ignores this setting because a container already starts from nothing
//! but its image — there is no host disk to jail.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FsMode {
    /// Only the sandbox directory, its mounts, and the system libraries.
    Strict,
    /// `Strict` plus the paths configured in app settings.
    Allowlist,
    /// The whole disk is readable except the credential stores.
    Open,
}

impl Default for FsMode {
    fn default() -> Self {
        // The safe end is the default: a new sandbox sees only what it was
        // explicitly given.
        FsMode::Strict
    }
}

impl FsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            FsMode::Strict => "strict",
            FsMode::Allowlist => "allowlist",
            FsMode::Open => "open",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "strict" => Some(FsMode::Strict),
            "allowlist" => Some(FsMode::Allowlist),
            "open" => Some(FsMode::Open),
            _ => None,
        }
    }

    /// True when reads outside the allowed set are denied.
    pub fn jails_reads(self) -> bool {
        matches!(self, FsMode::Strict | FsMode::Allowlist)
    }

    /// Short Vietnamese label for the UI and for tool results.
    pub fn label(self) -> &'static str {
        match self {
            FsMode::Strict => "Full isolation — only the sandbox and mounted folders are visible",
            FsMode::Allowlist => "Isolated plus the allowlist from settings",
            FsMode::Open => "Reads not isolated — the whole disk is readable (except secret folders)",
        }
    }
}

/// Directories an interpreter needs in order to exist at all.
///
/// Kept deliberately coarse: naming individual library directories means a
/// sandbox breaks the day the user installs Python somewhere else, and the
/// failure ("ImportError deep in the stdlib") reads as an app bug rather than a
/// policy decision.
#[cfg(target_os = "macos")]
pub const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/opt",
    "/private/etc",
    "/private/var/db",
    "/private/var/select",
    "/dev",
    "/Applications/Xcode.app",
];

#[cfg(not(target_os = "macos"))]
pub const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc",
    "/opt",
    "/run/current-system", // NixOS
    "/nix/store",
];

/// Every real path a sandbox may read, for a jailing mode.
///
/// Order is not significant to the caller — both backends consume this as a
/// set — but mounts and the workdir come first so a truncated debug print shows
/// the interesting entries.
pub fn read_roots(
    mode: FsMode,
    workdir: &str,
    mount_sources: &[String],
    allowlist: &[String],
) -> Vec<String> {
    let mut v: Vec<String> = vec![workdir.to_string()];
    v.extend(mount_sources.iter().cloned());
    if mode == FsMode::Allowlist {
        v.extend(allowlist.iter().filter(|p| !p.trim().is_empty()).cloned());
    }
    v.extend(SYSTEM_READ_ROOTS.iter().map(|s| s.to_string()));
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_isolating_one() {
        assert_eq!(FsMode::default(), FsMode::Strict);
        assert!(FsMode::default().jails_reads());
    }

    #[test]
    fn parse_round_trips_and_rejects_junk() {
        for m in [FsMode::Strict, FsMode::Allowlist, FsMode::Open] {
            assert_eq!(FsMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(FsMode::parse("STRICT"), Some(FsMode::Strict));
        assert_eq!(FsMode::parse("nope"), None);
        assert_eq!(FsMode::parse(""), None);
    }

    #[test]
    fn open_mode_does_not_jail_reads() {
        assert!(!FsMode::Open.jails_reads());
    }

    #[test]
    fn strict_sees_the_sandbox_its_mounts_and_the_system_but_not_the_allowlist() {
        let roots = read_roots(
            FsMode::Strict,
            "/w/sbx",
            &["/data/shared".into()],
            &["/Users/u/projects".into()],
        );
        assert!(roots.contains(&"/w/sbx".to_string()));
        assert!(roots.contains(&"/data/shared".to_string()));
        assert!(
            !roots.contains(&"/Users/u/projects".to_string()),
            "strict must ignore the allowlist — that is what allowlist mode is for"
        );
        assert!(roots.iter().any(|r| r == "/usr"), "an interpreter needs /usr");
    }

    #[test]
    fn allowlist_mode_adds_the_configured_paths() {
        let roots = read_roots(
            FsMode::Allowlist,
            "/w/sbx",
            &[],
            &["/Users/u/projects".into(), "  ".into()],
        );
        assert!(roots.contains(&"/Users/u/projects".to_string()));
        assert!(
            !roots.iter().any(|r| r.trim().is_empty()),
            "a blank allowlist entry would become an allow-everything rule"
        );
    }

    #[test]
    fn the_home_directory_is_never_a_system_root() {
        // If $HOME crept into SYSTEM_READ_ROOTS, strict mode would silently
        // become open mode.
        assert!(!SYSTEM_READ_ROOTS.iter().any(|r| *r == "/Users" || *r == "/home" || *r == "/"));
    }

    #[test]
    fn labels_are_populated_for_every_mode() {
        for m in [FsMode::Strict, FsMode::Allowlist, FsMode::Open] {
            assert!(m.label().len() > 10, "{:?} needs a label users can act on", m);
        }
    }
}

//! Platform keys a runtime package declares in `platforms`.
//!
//! One vocabulary for both sides: the daemon compares a package's list against
//! [`current`] to decide whether it is installable ("Compatible only" in the
//! Runtime screen), and release tooling names archives with the same keys.

/// Every key a manifest may use. Anything else is a validation error rather
/// than a package that silently never matches.
pub const KNOWN: &[&str] = &[
    "darwin-arm64",
    "darwin-x64",
    "linux-x64",
    "linux-arm64",
    "windows-x64",
    "windows-arm64",
];

/// The key for the machine this binary was compiled for.
pub fn current() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "darwin-arm64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "darwin-x64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-arm64"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "windows-arm64"
    } else if cfg!(target_os = "windows") {
        "windows-x64"
    } else {
        "linux-x64"
    }
}

/// Whether `key` names a platform this crate knows.
pub fn is_known(key: &str) -> bool {
    KNOWN.contains(&key)
}

/// File name suffix of an executable on this platform (`.exe` on Windows).
pub fn exe_suffix() -> &'static str {
    if cfg!(windows) {
        ".exe"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_is_a_known_key() {
        assert!(is_known(current()));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(!is_known("macos-arm64"));
        assert!(!is_known("darwin-aarch64"));
        assert!(!is_known(""));
    }
}

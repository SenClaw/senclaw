//! The skills that ship inside the binary.
//!
//! # Why these are embedded rather than read from a directory
//!
//! [`crate::config::Paths::bundled_skills_dir`] resolves to
//! `SENCLAW_BUNDLED_SKILLS_DIR`, or else to `CARGO_MANIFEST_DIR/skills` — a
//! path baked in at compile time. That points at the *build machine's* source
//! tree, so it resolves for a developer running out of the checkout and for
//! nobody else. A downloaded release binary looked for a directory that does
//! not exist on the user's disk and served **zero** builtin skills: `pattern`,
//! `web-research`, `agent-browser`, `code`, `wiki`, `workflow` and the rest
//! were simply absent from the registry, with nothing logged to say so.
//!
//! The fix is the one [`crate::patterns::catalog`] already uses for the starter
//! pattern library: the build script walks `skills/` and emits `include_bytes!`
//! calls, so the content travels *inside* the binary and cannot go missing.
//!
//! # Why unpack to disk instead of scanning from memory
//!
//! [`crate::skills::scan`] discovers skills by walking directories, and every
//! other source — clawhub-managed, `~/.claude/skills`, marketplace — is a real
//! directory. Teaching the scanner a second, in-memory shape would fork that
//! code path for one caller. Writing the embedded copy out once per binary
//! keeps one implementation, and leaves the files somewhere the user can read
//! when a skill misbehaves.

use std::fs;
use std::path::{Path, PathBuf};

/// One file of a bundled skill, pointed at the bytes compiled into the binary.
pub struct BundledSkillFile {
    /// Path relative to the skill's own directory, `/`-separated.
    pub rel: &'static str,
    pub bytes: &'static [u8],
    /// A skill may ship a helper script; losing the bit makes it unrunnable.
    pub executable: bool,
}

include!(concat!(env!("OUT_DIR"), "/bundled_skills.rs"));

/// Names of the skills compiled into this binary.
pub fn names() -> Vec<&'static str> {
    BUNDLED_SKILLS.iter().map(|(name, _)| *name).collect()
}

/// Stamp recording which build wrote the unpacked copy.
///
/// `SENCLAW_BUILD_EPOCH` changes on every build, so a new binary refreshes the
/// directory exactly once and an unchanged one never rewrites it.
const STAMP: &str = env!("SENCLAW_BUILD_EPOCH");

/// Unpack the embedded skills into `dir`, refreshing them when the binary that
/// wrote them was not this one.
///
/// The directory is owned by the binary, not the user: it is replaced wholesale
/// so a skill dropped upstream does not survive an upgrade. Hand edits belong in
/// a writable source (`~/.claude/skills`, clawhub-managed), which the scanner
/// reads separately and which this never touches.
pub fn unpack(dir: &Path) -> std::io::Result<()> {
    let stamp = dir.join(".build");
    if fs::read_to_string(&stamp).ok().as_deref() == Some(STAMP) {
        return Ok(());
    }

    // Stage beside the target and rename in, so a crash halfway through cannot
    // leave the scanner reading a half-written skill set. Same parent, so the
    // rename never crosses a filesystem.
    let staging = dir.with_extension("unpacking");
    let _ = fs::remove_dir_all(&staging);
    for (name, files) in BUNDLED_SKILLS {
        for file in *files {
            let path = staging.join(name).join(file.rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, file.bytes)?;
            set_executable(&path, file.executable)?;
        }
    }
    fs::write(staging.join(".build"), STAMP)?;

    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_dir_all(dir);
    fs::rename(&staging, dir)?;
    Ok(())
}

/// Unpack into `dir` and hand back the path, or `None` when the write failed.
///
/// A failure here must not take the daemon down: every other skill source is
/// still readable, and the warning names the cause.
pub fn ensure(dir: &Path) -> Option<PathBuf> {
    match unpack(dir) {
        Ok(()) => Some(dir.to_path_buf()),
        Err(e) => {
            tracing::warn!(
                "[skills] could not unpack the {} bundled skills into {}: {e}",
                BUNDLED_SKILLS.len(),
                dir.display()
            );
            None
        }
    }
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_skill_carries_a_skill_md() {
        assert!(
            BUNDLED_SKILLS.len() >= 15,
            "expected the builtin skill set, got {}",
            BUNDLED_SKILLS.len()
        );
        for (name, files) in BUNDLED_SKILLS {
            let md = files
                .iter()
                .find(|f| f.rel == "SKILL.md")
                .unwrap_or_else(|| panic!("skill \"{name}\" has no SKILL.md"));
            assert!(
                md.bytes.starts_with(b"---"),
                "skill \"{name}\" has no front matter"
            );
        }
    }

    /// The skill this module exists for: patterns reach the agent through it, so
    /// a release shipping without it makes the whole Patterns library
    /// unreachable from chat no matter how many patterns are installed.
    #[test]
    fn the_pattern_skill_is_in_the_binary() {
        let names = names();
        assert!(names.contains(&"pattern"), "got {names:?}");
        assert!(names.contains(&"web-research"), "got {names:?}");
    }

    #[test]
    fn unpack_writes_the_files_then_skips_the_second_call() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bundled-skills");

        unpack(&dir).unwrap();
        let skill = dir.join("pattern").join("SKILL.md");
        assert!(skill.is_file());
        assert!(fs::read_to_string(&skill).unwrap().contains("pattern"));

        // The second call should be a stamp read, not a rewrite: the marker
        // below survives only if nothing was touched.
        let marker = dir.join(".not-rewritten");
        fs::write(&marker, b"x").unwrap();
        unpack(&dir).unwrap();
        assert!(marker.is_file(), "an unchanged binary rewrote the directory");
    }

    #[test]
    fn a_stale_stamp_forces_a_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bundled-skills");
        unpack(&dir).unwrap();

        // What an upgrade looks like: same directory, different binary.
        fs::write(dir.join(".build"), "0").unwrap();
        let stale = dir.join("removed-upstream");
        fs::create_dir_all(&stale).unwrap();
        unpack(&dir).unwrap();

        assert!(
            !stale.exists(),
            "a skill dropped upstream survived the refresh"
        );
        assert!(dir.join("pattern").join("SKILL.md").is_file());
    }
}

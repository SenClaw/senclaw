//! Installed runtime packages on disk: `~/.senclaw/runtimes/<id>/<version>/`.
//!
//! A package is immutable once installed — updating means installing a new
//! version alongside the old one, never overwriting. The manifest
//! (`senclaw-runtime.json`) is written LAST, by extracting into a temp
//! directory first and renaming the whole directory into place: either the
//! full package appears atomically or nothing does, so a crash mid-install
//! never leaves a directory that looks installed.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use sen_runtime_sdk::manifest::{RuntimeManifest, MANIFEST_FILE};

/// Where a package came from — shown in the Runtime screen. A user-installed
/// copy wins over a bundled one at the same id+version (§2.1: "a
/// user-installed copy of the same id+version wins").
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageSource {
    Index,
    Bundled,
    Local,
}

#[derive(Debug, Clone)]
pub struct InstalledPackage {
    pub manifest: RuntimeManifest,
    pub warnings: Vec<String>,
    pub dir: PathBuf,
    pub source: PackageSource,
}

/// Scan one root (`<root>/<id>/<version>/senclaw-runtime.json`) for installed
/// packages. A version directory without a manifest is an interrupted
/// install — skipped silently, never reported as a broken runtime.
pub fn scan_root(root: &Path, source: PackageSource) -> Vec<InstalledPackage> {
    let mut out = Vec::new();
    let Ok(ids) = std::fs::read_dir(root) else {
        return out;
    };
    for id_entry in ids.filter_map(Result::ok) {
        let id_dir = id_entry.path();
        if !id_dir.is_dir() {
            continue;
        }
        let Ok(versions) = std::fs::read_dir(&id_dir) else {
            continue;
        };
        for v_entry in versions.filter_map(Result::ok) {
            let v_dir = v_entry.path();
            if !v_dir.is_dir() || !v_dir.join(MANIFEST_FILE).is_file() {
                continue;
            }
            // `finish_install` marks a locally-installed package on disk
            // (`SOURCE_MARKER_FILE`) since nothing else here can tell a user's
            // `install-local` apart from a catalog download once both sit in
            // the same root — a scan is the only place that marker is read.
            let resolved_source =
                if source == PackageSource::Index && v_dir.join(SOURCE_MARKER_FILE).is_file() { PackageSource::Local } else { source };
            match RuntimeManifest::read_from_dir(&v_dir) {
                Ok(parsed) => out.push(InstalledPackage {
                    manifest: parsed.manifest,
                    warnings: parsed.warnings,
                    dir: v_dir,
                    source: resolved_source,
                }),
                Err(e) => tracing::warn!(
                    "[runtime] {}/{MANIFEST_FILE} is not a valid manifest: {e}",
                    v_dir.display()
                ),
            }
        }
    }
    out
}

/// The user root plus, when set, the read-only bundled root — merged so a
/// user-installed copy of the same id+version shadows the bundled one.
pub fn scan_all(user_root: &Path, bundled_root: Option<&Path>) -> Vec<InstalledPackage> {
    let mut installed = scan_root(user_root, PackageSource::Index);
    if let Some(bundled_root) = bundled_root {
        for pkg in scan_root(bundled_root, PackageSource::Bundled) {
            let shadowed = installed
                .iter()
                .any(|p| p.manifest.id == pkg.manifest.id && p.manifest.version == pkg.manifest.version);
            if !shadowed {
                installed.push(pkg);
            }
        }
    }
    installed
}

/// A path safe to extract under some root: no `..`, no absolute/rooted
/// component, not empty. Applied to every archive entry so a package can only
/// ever write inside its own install directory — the same rule
/// `RuntimeManifest::validate` applies to `entry.command`, extended to every
/// file the archive carries.
pub fn safe_relative_path(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Copy a directory tree, following the same traversal guard as archive
/// extraction (a symlink inside the source that points outside it must not be
/// followed into the install). `root` is the package root the whole copy
/// started from — needed to compute a symlink's path relative to it, the same
/// way an archive entry's path is relative to the package root, regardless of
/// how deep `src`/`dst` are in the recursion.
///
/// A symlink that stays inside the package is recreated as a symlink (not
/// followed, not silently dropped) — upstream llama.cpp's macOS build ships
/// in-package dylib symlinks (`libllama.0.dylib -> libllama.0.5.0.dylib`), and
/// `install-local` on an already-extracted directory used to drop them,
/// leaving the runtime unable to load its own libraries. One escaping outside
/// the package is refused outright, same as the archive path.
fn copy_dir_all(root: &Path, src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let entry_path = entry.path();
        let dest = dst.join(entry.file_name());
        if file_type.is_symlink() {
            let target = std::fs::read_link(&entry_path).with_context(|| format!("read symlink {}", entry_path.display()))?;
            let rel = entry_path.strip_prefix(root).unwrap_or(&entry_path).to_path_buf();
            if !symlink_stays_inside(&rel, &target) {
                bail!("`{}` links to `{}`, outside the package", rel.display(), target.display());
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &dest).with_context(|| format!("recreate symlink {}", dest.display()))?;
            // Non-unix: validated above (an escaping link is still refused),
            // but not recreated — creating a symlink on Windows needs
            // elevated privileges and knowing file-vs-directory ahead of
            // time, and no shipped runtime package carries one today.
        } else if file_type.is_dir() {
            copy_dir_all(root, &entry_path, &dest)?;
        } else if file_type.is_file() {
            std::fs::copy(&entry_path, &dest)?;
        }
    }
    Ok(())
}

/// Give the manifest's `entry.command` the executable bit on Unix, regardless
/// of what permission bits the archive shipped it with — belt and suspenders
/// against a `.zip` built without the exec bit set.
fn mark_entry_executable(dir: &Path, manifest: &RuntimeManifest) -> Result<()> {
    let path = manifest.command_path(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.is_file() {
            let mut perm = std::fs::metadata(&path)?.permissions();
            perm.set_mode(perm.mode() | 0o111);
            std::fs::set_permissions(&path, perm)?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Locate `senclaw-runtime.json` either at `dir`'s root or one directory down
/// (§2.3: "manifest at the top level of the archive or inside a single
/// top-level directory"). Returns the directory that is actually the package
/// root.
fn locate_package_root(dir: &Path) -> Result<PathBuf> {
    if dir.join(MANIFEST_FILE).is_file() {
        return Ok(dir.to_path_buf());
    }
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(dir).context("read extracted archive")? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            candidates.push(entry.path());
        }
    }
    if candidates.len() == 1 && candidates[0].join(MANIFEST_FILE).is_file() {
        return Ok(candidates.remove(0));
    }
    bail!("no {MANIFEST_FILE} at the archive root or inside a single top-level directory")
}

/// A scratch directory under `runtimes_dir` guaranteed to be on the same
/// filesystem, so the final rename-into-place is atomic.
pub(crate) fn scratch_dir(runtimes_dir: &Path) -> Result<PathBuf> {
    let tmp_root = runtimes_dir.join(".tmp");
    std::fs::create_dir_all(&tmp_root)?;
    let dir = tmp_root.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The marker `scan_root` reads back to tell a locally-installed package
/// apart from a catalog download once both sit in the same `runtimes_dir` —
/// `source` on the value `finish_install` returns is otherwise a one-shot
/// label nothing persists, so the very next `GET /api/runtimes` would relabel
/// every package "index" regardless of how it actually arrived.
const SOURCE_MARKER_FILE: &str = ".install-source";

/// Validate the manifest in `staged` and rename the whole directory into
/// `<runtimes_dir>/<id>/<version>/`. Refuses (and cleans up `staged`) when
/// that slot is already occupied — uninstall first to replace it. `source` is
/// `Local` for `install-local` (a directory or archive the *user* pointed at)
/// and `Index` for anything installed from the catalog (`POST
/// /api/runtimes/install`, including the upstream llama.cpp resolver) —
/// never `Bundled`, which is a read-only root `finish_install` never writes
/// into.
pub(crate) fn finish_install(runtimes_dir: &Path, staged: PathBuf, source: PackageSource) -> Result<InstalledPackage> {
    let cleanup = |staged: &Path| {
        let _ = std::fs::remove_dir_all(staged);
    };
    let parsed = match RuntimeManifest::read_from_dir(&staged) {
        Ok(p) => p,
        Err(e) => {
            cleanup(&staged);
            bail!("invalid manifest: {e}");
        }
    };
    let target = runtimes_dir.join(&parsed.manifest.id).join(&parsed.manifest.version);
    if target.exists() {
        cleanup(&staged);
        bail!(
            "{} {} is already installed — uninstall it first to replace it",
            parsed.manifest.id,
            parsed.manifest.version
        );
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Err(e) = mark_entry_executable(&staged, &parsed.manifest) {
        tracing::warn!(
            "[runtime] could not mark {} executable: {e:#}",
            parsed.manifest.entry.command
        );
    }
    if source == PackageSource::Local {
        // Best-effort: a write failure here costs only a mislabeled `source`
        // in the UI, never the install itself.
        let _ = std::fs::write(staged.join(SOURCE_MARKER_FILE), "local");
    }
    std::fs::rename(&staged, &target).with_context(|| format!("install into {}", target.display()))?;
    Ok(InstalledPackage { manifest: parsed.manifest, warnings: parsed.warnings, dir: target, source })
}

/// Install from a directory holding `senclaw-runtime.json` (at its root, or
/// one level down). Copies into a scratch directory first so a bad manifest
/// or a mid-copy failure never touches `runtimes_dir`.
pub fn install_from_dir(runtimes_dir: &Path, src: &Path, source: PackageSource) -> Result<InstalledPackage> {
    if !src.is_dir() {
        bail!("{} is not a directory", src.display());
    }
    let package_root = locate_package_root(src)?;
    let staged_parent = scratch_dir(runtimes_dir)?;
    let staged = staged_parent.join("pkg");
    copy_dir_all(&package_root, &package_root, &staged)?;
    let result = finish_install(runtimes_dir, staged, source);
    let _ = std::fs::remove_dir_all(&staged_parent);
    result
}

/// Whether a symlink at `rel` (relative to the package root) pointing at
/// `target` resolves inside the package. Only a relative target that never
/// climbs above the root qualifies — resolved lexically, because the target
/// may not have been extracted yet.
fn symlink_stays_inside(rel: &Path, target: &Path) -> bool {
    use std::path::Component;
    if target.is_absolute() {
        return false;
    }
    let mut depth: usize = 0;
    for comp in rel.parent().unwrap_or(Path::new("")).components().chain(target.components()) {
        match comp {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Extract a `.tar.gz` archive into `dest`, rejecting any entry whose path
/// would escape `dest`, any symlink whose target does (a link is how an
/// archive could point outside itself without its path saying so), and hard
/// links outright.
///
/// Symlinks that stay inside are allowed because real packages carry them:
/// upstream llama.cpp's macOS build ships `libllama.0.dylib →
/// libllama.0.5.0.dylib` and friends, and refusing every link made it
/// uninstallable. Hard links stay refused: `tar` resolves their target
/// against the process's working directory, not `dest`.
pub(crate) fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        let entry_type = entry.header().entry_type();
        let raw_path = entry.path().context("read entry path")?.into_owned();
        if entry_type.is_hard_link() {
            bail!("archive entry {} is a hard link, which packages may not contain", raw_path.display());
        }
        if entry_type.is_dir() {
            continue; // directories are created implicitly by file entries below
        }
        let Some(rel) = safe_relative_path(&raw_path) else {
            bail!("archive entry `{}` escapes the package", raw_path.display());
        };
        if entry_type.is_symlink() {
            let target = entry.link_name().context("read link target")?.map(|t| t.into_owned()).unwrap_or_default();
            if target.as_os_str().is_empty() || !symlink_stays_inside(&rel, &target) {
                bail!(
                    "archive entry {} links to `{}`, outside the package",
                    rel.display(),
                    target.display()
                );
            }
        }
        // `unpack_in` re-derives the destination from the entry's own
        // header path and canonicalizes it against `dest`
        // (`validate_inside_dst`) before writing — the tar crate's own
        // traversal guard, as defense in depth *on top of*
        // `safe_relative_path`/`symlink_stays_inside` above, not instead of
        // them. The plain `entry.unpack(&out_path)` this replaces
        // (`target_base: None`) skips that canonicalization entirely.
        let unpacked = entry.unpack_in(dest).with_context(|| format!("extract {}", rel.display()))?;
        if !unpacked {
            bail!("archive entry `{}` was refused by the extractor's own path check", rel.display());
        }
    }
    Ok(())
}

/// Extract a `.zip` archive into `dest` with the same traversal guard.
pub(crate) fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("read zip archive")?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).context("read zip entry")?;
        let raw_name = entry.name().to_string();
        let Some(enclosed) = entry.enclosed_name() else {
            bail!("archive entry `{raw_name}` escapes the package");
        };
        let Some(rel) = safe_relative_path(&enclosed) else {
            bail!("archive entry `{raw_name}` escapes the package");
        };
        if entry.is_dir() {
            std::fs::create_dir_all(dest.join(&rel))?;
            continue;
        }
        let out_path = dest.join(&rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&out_path).with_context(|| format!("create {}", out_path.display()))?;
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf)?;
        std::io::Write::write_all(&mut out, &buf)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

/// Install from a `.tar.gz` or `.zip` release archive (§2.3).
pub fn install_from_archive(runtimes_dir: &Path, archive: &Path, source: PackageSource) -> Result<InstalledPackage> {
    let name = archive.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let staged_parent = scratch_dir(runtimes_dir)?;
    let extracted = staged_parent.join("extracted");
    std::fs::create_dir_all(&extracted)?;

    let extraction = if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        extract_tar_gz(archive, &extracted)
    } else if name.ends_with(".zip") {
        extract_zip(archive, &extracted)
    } else {
        Err(anyhow::anyhow!("`{name}` is neither a .tar.gz nor a .zip archive"))
    };
    if let Err(e) = extraction {
        let _ = std::fs::remove_dir_all(&staged_parent);
        return Err(e);
    }

    let package_root = match locate_package_root(&extracted) {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staged_parent);
            return Err(e);
        }
    };
    // Move just the package root out from under the scratch parent so
    // `finish_install`'s rename lands exactly the package contents.
    let staged = staged_parent.join("pkg");
    std::fs::rename(&package_root, &staged)?;
    let result = finish_install(runtimes_dir, staged, source);
    let _ = std::fs::remove_dir_all(&staged_parent);
    result
}

/// Install from either a directory or a `.tar.gz`/`.zip` archive — what
/// `POST /api/runtimes/install-local` and `senclaw runtime install-local`
/// both accept. Always `PackageSource::Local`: this is the entry point for a
/// path the *user* pointed at, never the catalog.
pub fn install_local(runtimes_dir: &Path, path: &Path) -> Result<InstalledPackage> {
    if path.is_dir() {
        install_from_dir(runtimes_dir, path, PackageSource::Local)
    } else if path.is_file() {
        install_from_archive(runtimes_dir, path, PackageSource::Local)
    } else {
        bail!("{} does not exist", path.display())
    }
}

/// Remove an installed package. The caller is responsible for stopping any
/// running process first (`docs/runtime-protocol.md §5.1`: 409 while running
/// unless `force`).
pub fn uninstall(runtimes_dir: &Path, id: &str, version: &str) -> Result<()> {
    if !sen_runtime_sdk::manifest::valid_id(id) || !sen_runtime_sdk::manifest::valid_version(version) {
        bail!("`{id}` `{version}` is not a valid package identity");
    }
    let dir = runtimes_dir.join(id).join(version);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    // Clean up the id directory once it holds no more versions, so an
    // uninstall-everything leaves no empty shell behind in the Runtime screen.
    if let Some(id_dir) = dir.parent() {
        if id_dir.read_dir().map(|mut it| it.next().is_none()).unwrap_or(false) {
            let _ = std::fs::remove_dir(id_dir);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &Path, id: &str, version: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("bin").join("run"), "#!/bin/sh\necho hi\n").unwrap_or_else(|_| {
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::write(dir.join("bin").join("run"), "#!/bin/sh\necho hi\n").unwrap();
        });
        let manifest = format!(
            r#"{{
              "schemaVersion": 1, "id": "{id}", "name": "Test", "version": "{version}",
              "type": "ocr", "slots": ["ocr"], "capabilities": ["ocr"],
              "platforms": ["darwin-arm64","darwin-x64","linux-x64","linux-arm64","windows-x64","windows-arm64"],
              "mode": "service",
              "entry": {{"command": "bin/run", "args": ["serve", "--port", "{{port}}"]}}
            }}"#
        );
        std::fs::write(dir.join(MANIFEST_FILE), manifest).unwrap();
    }

    #[test]
    fn safe_relative_path_rejects_escapes() {
        assert!(safe_relative_path(Path::new("bin/run")).is_some());
        assert!(safe_relative_path(Path::new("./bin/run")).is_some());
        assert!(safe_relative_path(Path::new("../x")).is_none());
        assert!(safe_relative_path(Path::new("a/../../x")).is_none());
        assert!(safe_relative_path(Path::new("/etc/passwd")).is_none());
        assert!(safe_relative_path(Path::new("")).is_none());
    }

    #[test]
    fn install_from_dir_then_scan_then_uninstall() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src-pkg");
        write_manifest(&src, "sen-ocr", "0.1.0");
        let runtimes_dir = tmp.path().join("runtimes");

        let installed = install_from_dir(&runtimes_dir, &src, PackageSource::Local).unwrap();
        assert_eq!(installed.manifest.id, "sen-ocr");
        assert!(installed.dir.join(MANIFEST_FILE).is_file());
        assert!(!installed.dir.join(".tmp").exists());

        let scanned = scan_root(&runtimes_dir, PackageSource::Index);
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].manifest.version, "0.1.0");
        assert_eq!(scanned[0].source, PackageSource::Local, "the marker survives a fresh scan");

        // Same id+version refused without uninstalling first.
        assert!(install_from_dir(&runtimes_dir, &src, PackageSource::Local).is_err());

        uninstall(&runtimes_dir, "sen-ocr", "0.1.0").unwrap();
        assert!(scan_root(&runtimes_dir, PackageSource::Index).is_empty());
        assert!(!runtimes_dir.join("sen-ocr").exists(), "empty id directory is cleaned up");
    }

    #[test]
    fn install_from_dir_accepts_a_single_top_level_wrapper_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let wrapper = tmp.path().join("wrapper");
        write_manifest(&wrapper.join("sen-ocr-0.1.0"), "sen-ocr", "0.1.0");
        let runtimes_dir = tmp.path().join("runtimes");

        let installed = install_from_dir(&runtimes_dir, &wrapper, PackageSource::Local).unwrap();
        assert_eq!(installed.manifest.id, "sen-ocr");
    }

    #[test]
    fn bundled_root_is_shadowed_by_a_user_install_of_the_same_version() {
        let tmp = tempfile::tempdir().unwrap();
        let user_root = tmp.path().join("runtimes");
        let bundled_root = tmp.path().join("bundled");
        write_manifest(&bundled_root.join("sen-ocr").join("0.1.0"), "sen-ocr", "0.1.0");
        write_manifest(&bundled_root.join("sen-tts").join("0.1.0"), "sen-tts", "0.1.0");
        install_from_dir(
            &user_root,
            &{
                let d = tmp.path().join("src-ocr");
                write_manifest(&d, "sen-ocr", "0.1.0");
                d
            },
            PackageSource::Index,
        )
        .unwrap();

        let merged = scan_all(&user_root, Some(&bundled_root));
        assert_eq!(merged.len(), 2, "one shadowed sen-ocr + the unique sen-tts");
        let ocr = merged.iter().find(|p| p.manifest.id == "sen-ocr").unwrap();
        assert_eq!(ocr.source, PackageSource::Index, "the user copy wins, not the bundled one");
    }

    #[test]
    fn install_from_archive_rejects_a_traversal_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("stage");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("evil.txt"), b"pwned").unwrap();
        let zip_path = tmp.path().join("evil.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            writer.start_file("../escaped.txt", opts).unwrap();
            std::io::Write::write_all(&mut writer, b"pwned").unwrap();
            writer.finish().unwrap();
        }
        let runtimes_dir = tmp.path().join("runtimes");
        let err = install_from_archive(&runtimes_dir, &zip_path, PackageSource::Index).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
        assert!(!tmp.path().join("escaped.txt").exists(), "must never land outside the temp extraction dir");
    }

    #[test]
    fn install_from_archive_extracts_a_real_tar_gz() {
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("stage");
        write_manifest(&stage, "sen-tts", "0.2.0");
        let tar_gz = tmp.path().join("pkg.tar.gz");
        {
            let file = std::fs::File::create(&tar_gz).unwrap();
            let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut builder = tar::Builder::new(enc);
            builder.append_dir_all(".", &stage).unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let runtimes_dir = tmp.path().join("runtimes");
        let installed = install_from_archive(&runtimes_dir, &tar_gz, PackageSource::Index).unwrap();
        assert_eq!(installed.manifest.id, "sen-tts");
        assert_eq!(installed.manifest.version, "0.2.0");
    }

    /// A tar.gz holding the manifest, one real file, and one symlink entry.
    #[cfg(unix)]
    fn tar_with_link(tmp: &Path, link_name: &str, target: &str) -> PathBuf {
        let stage = tmp.join("stage");
        write_manifest(&stage, "llama.cpp-metal", "b11201");
        std::fs::write(stage.join("libllama.0.5.0.dylib"), b"dylib").unwrap();
        let tar_gz = tmp.join("pkg.tar.gz");
        let file = std::fs::File::create(&tar_gz).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);
        builder.append_dir_all(".", &stage).unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder.append_link(&mut header, link_name, target).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        tar_gz
    }

    /// Upstream llama.cpp versions its dylibs with in-package symlinks; refusing
    /// every link made the GGUF runtime impossible to install.
    #[cfg(unix)]
    #[test]
    fn a_symlink_that_stays_inside_the_package_is_extracted() {
        let tmp = tempfile::tempdir().unwrap();
        let tar_gz = tar_with_link(tmp.path(), "libllama.0.dylib", "libllama.0.5.0.dylib");
        let installed = install_from_archive(&tmp.path().join("runtimes"), &tar_gz, PackageSource::Index).unwrap();
        let link = installed.dir.join("libllama.0.dylib");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&link).unwrap(), b"dylib", "the link must resolve to the packaged file");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_escapes_the_package_is_refused() {
        for target in ["../../../etc/passwd", "/etc/passwd", "sub/../../outside"] {
            let tmp = tempfile::tempdir().unwrap();
            let tar_gz = tar_with_link(tmp.path(), "escape", target);
            let err = install_from_archive(&tmp.path().join("runtimes"), &tar_gz, PackageSource::Index).unwrap_err();
            assert!(err.to_string().contains("outside the package"), "{target}: {err}");
        }
    }

    #[test]
    fn symlink_confinement_is_resolved_against_the_link_directory() {
        assert!(symlink_stays_inside(Path::new("lib/a.dylib"), Path::new("a.1.dylib")));
        assert!(symlink_stays_inside(Path::new("lib/a.dylib"), Path::new("../bin/tool")));
        assert!(!symlink_stays_inside(Path::new("a.dylib"), Path::new("../a.dylib")));
        assert!(!symlink_stays_inside(Path::new("lib/a"), Path::new("../../x")));
        assert!(!symlink_stays_inside(Path::new("a"), Path::new("/usr/lib/libz.dylib")));
    }

    /// `install-local` on an already-extracted directory (not an archive)
    /// used to silently drop every symlink — upstream llama.cpp's macOS build
    /// ships in-package dylib symlinks that the runtime needs to load.
    #[cfg(unix)]
    #[test]
    fn install_from_dir_recreates_a_symlink_that_stays_inside_the_package() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src-pkg");
        write_manifest(&src, "llama.cpp-metal", "b11201");
        std::fs::write(src.join("libllama.0.5.0.dylib"), b"dylib").unwrap();
        std::os::unix::fs::symlink("libllama.0.5.0.dylib", src.join("libllama.0.dylib")).unwrap();

        let runtimes_dir = tmp.path().join("runtimes");
        let installed = install_from_dir(&runtimes_dir, &src, PackageSource::Local).unwrap();

        let link = installed.dir.join("libllama.0.dylib");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the symlink must survive install-local, not be dropped");
        assert_eq!(std::fs::read(&link).unwrap(), b"dylib");
    }

    #[cfg(unix)]
    #[test]
    fn install_from_dir_refuses_a_symlink_that_escapes_the_package() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src-pkg");
        write_manifest(&src, "sen-ocr", "0.1.0");
        std::os::unix::fs::symlink("/etc/passwd", src.join("evil")).unwrap();

        let runtimes_dir = tmp.path().join("runtimes");
        let err = install_from_dir(&runtimes_dir, &src, PackageSource::Local).unwrap_err();
        assert!(err.to_string().contains("outside the package"), "{err}");
    }

    /// Nested: the same guard must apply at any depth, not just at the
    /// package root — `rel` is computed relative to `root` across recursion.
    #[cfg(unix)]
    #[test]
    fn install_from_dir_recreates_a_nested_symlink_that_stays_inside_the_package() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src-pkg");
        write_manifest(&src, "llama.cpp-metal", "b11201");
        std::fs::create_dir_all(src.join("lib")).unwrap();
        std::fs::write(src.join("lib").join("libllama.0.5.0.dylib"), b"dylib").unwrap();
        std::os::unix::fs::symlink("libllama.0.5.0.dylib", src.join("lib").join("libllama.0.dylib")).unwrap();

        let runtimes_dir = tmp.path().join("runtimes");
        let installed = install_from_dir(&runtimes_dir, &src, PackageSource::Local).unwrap();

        let link = installed.dir.join("lib").join("libllama.0.dylib");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&link).unwrap(), b"dylib");
    }
}

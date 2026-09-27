//! ClawHub local install state management. Mirrors `src-old/clawhub/lockfile.ts`.
//!
//! lockfile:   ~/.senclaw/managed/skills/.clawhub/lock.json
//! origin:     ~/.senclaw/managed/skills/<slug>/.clawhub/origin.json

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ===== Types =====

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockfileEntry {
    pub version: Option<String>,
    #[serde(rename = "installedAt")]
    pub installed_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lockfile {
    pub version: u8,
    pub skills: HashMap<String, LockfileEntry>,
}

impl Default for Lockfile {
    fn default() -> Self {
        Self {
            version: 1,
            skills: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillOrigin {
    pub version: u8,
    pub registry: String,
    pub slug: String,
    #[serde(rename = "installedVersion")]
    pub installed_version: String,
    #[serde(rename = "installedAt")]
    pub installed_at: u64,
}

const DOT_DIR: &str = ".clawhub";

// ===== Lockfile =====

fn lockfile_path(managed_skills_dir: &Path) -> PathBuf {
    managed_skills_dir.join(DOT_DIR).join("lock.json")
}

pub fn read_lockfile(managed_skills_dir: &Path) -> Lockfile {
    let path = lockfile_path(managed_skills_dir);
    match fs::read_to_string(&path) {
        Ok(raw) => {
            let parsed: Option<Lockfile> = serde_json::from_str(&raw).ok();
            match parsed {
                Some(lf) if lf.version == 1 => lf,
                _ => Lockfile::default(),
            }
        }
        Err(_) => Lockfile::default(),
    }
}

pub fn write_lockfile(managed_skills_dir: &Path, lock: &Lockfile) -> Result<(), anyhow::Error> {
    let p = lockfile_path(managed_skills_dir);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(lock)?;
    fs::write(&p, json + "\n")?;
    Ok(())
}

// ===== Skill Origin =====

fn origin_path(skill_folder: &Path) -> PathBuf {
    skill_folder.join(DOT_DIR).join("origin.json")
}

pub fn read_skill_origin(skill_folder: &Path) -> Option<SkillOrigin> {
    let raw = fs::read_to_string(origin_path(skill_folder)).ok()?;
    let parsed: SkillOrigin = serde_json::from_str(&raw).ok()?;
    if parsed.version != 1
        || parsed.registry.is_empty()
        || parsed.slug.is_empty()
        || parsed.installed_version.is_empty()
    {
        return None;
    }
    Some(parsed)
}

pub fn write_skill_origin(skill_folder: &Path, origin: &SkillOrigin) -> Result<(), anyhow::Error> {
    let p = origin_path(skill_folder);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(origin)?;
    fs::write(&p, json + "\n")?;
    Ok(())
}

// ===== Path sanitization =====

fn sanitize_rel_path(p: &str) -> Option<&str> {
    let normalized = p.trim_start_matches("./").trim_start_matches('/');
    if normalized.is_empty() || normalized.ends_with('/') {
        return None;
    }
    if normalized.contains("..") || normalized.contains('\\') {
        return None;
    }
    Some(normalized)
}

// ===== Zip extract =====

/// Extract a zip file (as bytes) into `target_dir`.
pub fn extract_zip_to_dir(zip_bytes: &[u8], target_dir: &Path) -> Result<(), anyhow::Error> {
    let cursor = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor)?;
    fs::create_dir_all(target_dir)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let raw_path = file.name().to_string();
        let safe_path = match sanitize_rel_path(&raw_path) {
            Some(p) => p,
            None => continue,
        };
        let out_path = target_dir.join(safe_path);
        if file.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = fs::File::create(&out_path)?;
            std::io::copy(&mut file, &mut out)?;

            #[cfg(unix)]
            {
                if let Some(mode) = file.unix_mode() {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(mut perms) = fs::metadata(&out_path).map(|m| m.permissions()) {
                        perms.set_mode(mode);
                        let _ = fs::set_permissions(&out_path, perms);
                    }
                }
            }
        }
    }
    Ok(())
}

// ===== Zip create =====

/// Collect all files under `dir` into a map of relative path → bytes, excluding `.clawhub/`.
pub fn collect_dir_files(dir: &Path) -> Result<HashMap<String, Vec<u8>>, anyhow::Error> {
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    collect_recursive(dir, dir, &mut files)?;
    Ok(files)
}

fn collect_recursive(
    base_dir: &Path,
    current_dir: &Path,
    files: &mut HashMap<String, Vec<u8>>,
) -> Result<(), anyhow::Error> {
    for entry in fs::read_dir(current_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".clawhub" {
            continue;
        }
        let full_path = entry.path();
        let rel_path = full_path
            .strip_prefix(base_dir)?
            .to_string_lossy()
            .replace('\\', "/");
        if full_path.is_dir() {
            collect_recursive(base_dir, &full_path, files)?;
        } else {
            let data = fs::read(&full_path)?;
            files.insert(rel_path, data);
        }
    }
    Ok(())
}

/// Create a zip file from a map of path → bytes.
pub fn zip_files(files: &HashMap<String, Vec<u8>>) -> Result<Vec<u8>, anyhow::Error> {
    let mut buf = Vec::new();
    {
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in files {
            archive.start_file(name, options)?;
            std::io::Write::write_all(&mut archive, data)?;
        }
        archive.finish()?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lockfile_default() {
        let lf = Lockfile::default();
        assert_eq!(lf.version, 1);
        assert!(lf.skills.is_empty());
    }

    #[test]
    fn test_lockfile_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("test-lockfile-{}", uuid::Uuid::new_v4()));
        let mut lf = Lockfile::default();
        lf.skills.insert(
            "test-skill".to_string(),
            LockfileEntry {
                version: Some("1.0.0".to_string()),
                installed_at: 1700000000,
            },
        );
        write_lockfile(&tmp, &lf).unwrap();
        let read = read_lockfile(&tmp);
        assert_eq!(read.skills.len(), 1);
        assert_eq!(
            read.skills
                .get("test-skill")
                .and_then(|e| e.version.as_deref()),
            Some("1.0.0")
        );
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_lockfile_missing_dir_returns_default() {
        let nonexistent = Path::new("/nonexistent/lock/dir");
        let lf = read_lockfile(nonexistent);
        assert_eq!(lf.version, 1);
        assert!(lf.skills.is_empty());
    }

    #[test]
    fn test_sanitize_rel_path() {
        assert_eq!(sanitize_rel_path("foo/bar.md"), Some("foo/bar.md"));
        assert_eq!(sanitize_rel_path("./foo/bar.md"), Some("foo/bar.md"));
        assert_eq!(sanitize_rel_path("foo/"), None);
        assert_eq!(sanitize_rel_path("../foo"), None);
        assert_eq!(sanitize_rel_path("foo\\bar"), None);
        assert_eq!(sanitize_rel_path(""), None);
    }

    #[test]
    fn test_skill_origin_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("test-origin-{}", uuid::Uuid::new_v4()));
        let origin = SkillOrigin {
            version: 1,
            registry: "https://example.com".to_string(),
            slug: "my-skill".to_string(),
            installed_version: "2.0.0".to_string(),
            installed_at: 1700000000,
        };
        write_skill_origin(&tmp, &origin).unwrap();
        let read = read_skill_origin(&tmp).unwrap();
        assert_eq!(read.slug, "my-skill");
        assert_eq!(read.registry, "https://example.com");
        assert_eq!(read.installed_version, "2.0.0");
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_zip_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("test-zip-{}", uuid::Uuid::new_v4()));
        let source_dir = tmp.join("source");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("hello.txt"), b"hello world").unwrap();
        fs::create_dir_all(source_dir.join("sub")).unwrap();
        fs::write(source_dir.join("sub").join("nested.txt"), b"nested").unwrap();

        let files = collect_dir_files(&source_dir).unwrap();
        assert_eq!(files.len(), 2);

        let zip_bytes = zip_files(&files).unwrap();
        assert!(!zip_bytes.is_empty());

        let out_dir = tmp.join("extracted");
        extract_zip_to_dir(&zip_bytes, &out_dir).unwrap();

        let hello = fs::read_to_string(out_dir.join("hello.txt")).unwrap();
        assert_eq!(hello, "hello world");
        let nested = fs::read_to_string(out_dir.join("sub").join("nested.txt")).unwrap();
        assert_eq!(nested, "nested");

        fs::remove_dir_all(&tmp).ok();
    }
}

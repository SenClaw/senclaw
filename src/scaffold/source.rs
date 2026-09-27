//! Where a template comes from, and how it is loaded into memory.
//!
//! Three sources, tried in this order:
//!
//! 1. **A local path** (`--template ./my-template`) — for authoring a template,
//!    or for an organisation that keeps its own next to the code.
//! 2. **Git** — the default. The repo is cloned once into
//!    [`crate::config::PathsConfig::scaffold_templates_dir`] and pulled on
//!    subsequent runs, so a template fix reaches everyone without a release.
//! 3. **Bundled** — the copies compiled into the binary. This is the fallback,
//!    and it is what makes `senclaw create app` a command that always works:
//!    on a plane, behind a proxy that eats git, or before the templates repo
//!    exists at all.
//!
//! A git failure is *not* fatal when a bundled template of the same name
//! exists. It is reported and the bundled copy is used — the alternative is a
//! scaffolder that stops working when GitHub does.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::spec::{self, TemplateSpec};

/// The templates repo. Overridable per invocation with `--repo`, and globally
/// with `SENCLAW_TEMPLATES_REPO`, so a fork or an internal mirror needs no
/// rebuild.
pub const DEFAULT_TEMPLATE_REPO: &str = "https://github.com/NortonBen/senclaw-templates";

/// Branch cloned when `--ref` is not given.
pub const DEFAULT_TEMPLATE_REF: &str = "main";

/// One file of a template payload, already read.
///
/// Templates are small (a working single-file app plus a manifest), so loading
/// the whole payload into memory buys a simple two-phase create: render
/// everything, and only then touch the destination. A template that fails to
/// render leaves no half-written directory behind.
#[derive(Debug, Clone)]
pub struct TemplateFile {
    /// Path relative to the payload root, always `/`-separated.
    pub rel: String,
    pub bytes: Vec<u8>,
    /// Unix executable bit. Carried through because a template's
    /// `scripts/pack.sh` is useless without it.
    pub executable: bool,
}

/// A template loaded and ready to render.
pub struct LoadedTemplate {
    pub spec: TemplateSpec,
    pub files: Vec<TemplateFile>,
    /// Human-readable provenance, printed so the user always knows whether they
    /// got the repo's version or the built-in one.
    pub origin: String,
}

/// How to reach the templates repo.
#[derive(Debug, Clone)]
pub struct GitSource {
    pub repo: String,
    pub reference: String,
    pub cache_dir: PathBuf,
}

/// What the caller asked for.
#[derive(Debug, Clone)]
pub enum Requested {
    /// A template name, resolved against git then the bundled set.
    Named(String),
    /// An explicit directory on disk.
    Path(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefer {
    /// Try git first, fall back to bundled. The default.
    Git,
    /// Never touch the network.
    Offline,
    /// Refresh the clone even if it is already there.
    Refresh,
}

/// Load a template, reporting which source won.
///
/// `warn` receives non-fatal problems (a git failure that was survivable), so
/// the CLI can print them without this module knowing about stdout.
pub fn load(
    requested: &Requested,
    git: &GitSource,
    prefer: Prefer,
    warn: &mut dyn FnMut(String),
) -> Result<LoadedTemplate> {
    match requested {
        Requested::Path(p) => {
            if !p.is_dir() {
                bail!("không tìm thấy thư mục template: {}", p.display());
            }
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "template".to_string());
            load_from_dir(p, &name, format!("local:{}", p.display()))
        }
        Requested::Named(name) => {
            if prefer != Prefer::Offline {
                match sync_repo(git, prefer == Prefer::Refresh) {
                    Ok(()) => {
                        if let Some(dir) = find_in_repo(&git.cache_dir, name) {
                            let origin = format!("git:{}@{}", git.repo, git.reference);
                            return load_from_dir(&dir, name, origin);
                        }
                        // The clone worked but has no such template. Only worth
                        // saying when there is no bundled copy to fall back to,
                        // which the error path below covers.
                        if super::bundled::get(name).is_none() {
                            bail!(
                                "template {:?} không có trong {} (nhánh {}). \
                                 Xem danh sách: senclaw create list",
                                name,
                                git.repo,
                                git.reference
                            );
                        }
                    }
                    Err(e) => {
                        if super::bundled::get(name).is_none() {
                            return Err(e).with_context(|| {
                                format!("không lấy được template {name:?} từ {}", git.repo)
                            });
                        }
                        warn(format!(
                            "không đồng bộ được {} ({e}); dùng bản template đi kèm binary",
                            git.repo
                        ));
                    }
                }
            }

            let files = super::bundled::get(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "không có template {:?}. Có sẵn: {}",
                    name,
                    super::bundled::names().join(", ")
                )
            })?;
            load_from_bundled(name, files)
        }
    }
}

/// Clone or update the templates repo.
fn sync_repo(git: &GitSource, refresh: bool) -> Result<()> {
    if refresh && git.cache_dir.exists() {
        std::fs::remove_dir_all(&git.cache_dir).with_context(|| {
            format!("không xoá được cache template {}", git.cache_dir.display())
        })?;
    }
    crate::marketplace::git_sync::clone_or_pull(&git.repo, &git.reference, &git.cache_dir)
}

/// Find a template directory inside a cloned repo.
///
/// Both layouts are accepted — `templates/<name>/` (what the official repo
/// uses, leaving room for a README and CI at the root) and `<name>/` (what a
/// three-file fork looks like) — because the alternative is a repo that clones
/// fine and then reports every template missing.
pub fn find_in_repo(repo: &Path, name: &str) -> Option<PathBuf> {
    for base in [repo.join("templates"), repo.to_path_buf()] {
        let dir = base.join(name);
        if dir.is_dir() {
            return Some(dir);
        }
    }
    None
}

/// Every template name a cloned repo offers.
pub fn list_repo(repo: &Path) -> Vec<(String, PathBuf)> {
    let base = {
        let t = repo.join("templates");
        if t.is_dir() {
            t
        } else {
            repo.to_path_buf()
        }
    };
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&base) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let Some(name) = p.file_name().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        if name.starts_with('.') || spec::ALWAYS_IGNORED.contains(&name.as_str()) {
            continue;
        }
        // A directory is a template when it says so, or when it looks like one.
        if p.join("template.json").exists()
            || p.join("senclaw-manifest.json").exists()
            || p.join("SKILL.md").exists()
            || p.join("files").is_dir()
        {
            out.push((name, p));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn load_from_dir(dir: &Path, name: &str, origin: String) -> Result<LoadedTemplate> {
    let spec = TemplateSpec::load(dir, name)?;
    let root = spec::payload_root(dir, &spec);
    if !root.is_dir() {
        bail!(
            "template {name:?} khai báo root {:?} nhưng thư mục đó không tồn tại",
            root.display()
        );
    }
    let mut files = Vec::new();
    walk(&root, &root, &spec.ignore, &mut files)?;
    if files.is_empty() {
        bail!("template {name:?} rỗng (không có file nào để render)");
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(LoadedTemplate {
        spec,
        files,
        origin,
    })
}

fn load_from_bundled(name: &str, files: &[super::bundled::BundledFile]) -> Result<LoadedTemplate> {
    let spec_raw = files
        .iter()
        .find(|f| f.rel == "template.json")
        .map(|f| String::from_utf8_lossy(f.bytes).to_string());

    let spec = match spec_raw {
        Some(raw) => TemplateSpec::parse(&raw, name)
            .with_context(|| format!("template.json của template đi kèm {name:?} hỏng"))?,
        None => TemplateSpec {
            name: name.to_string(),
            ..TemplateSpec::inferred(name, Path::new(""))
        },
    };

    // Same payload-root rule as a directory template ([`spec::payload_root`]),
    // applied to flat paths: a `files/` prefix means the template's own README
    // and `template.json` sit outside the thing being created.
    let prefix = match spec.root.as_deref().map(str::trim) {
        Some(r) if !r.is_empty() => format!("{}/", r.trim_end_matches('/')),
        _ => {
            if files.iter().any(|f| f.rel.starts_with("files/")) {
                "files/".to_string()
            } else {
                String::new()
            }
        }
    };

    let payload: Vec<TemplateFile> = files
        .iter()
        .filter_map(|f| f.rel.strip_prefix(prefix.as_str()).map(|rel| (rel, f)))
        .filter(|(rel, _)| !spec::is_ignored(rel, &spec.ignore))
        .map(|(rel, f)| TemplateFile {
            rel: rel.to_string(),
            bytes: f.bytes.to_vec(),
            executable: f.executable,
        })
        .collect();

    if payload.is_empty() {
        bail!("template đi kèm {name:?} rỗng");
    }
    Ok(LoadedTemplate {
        spec,
        files: payload,
        origin: "bundled".to_string(),
    })
}

fn walk(root: &Path, dir: &Path, extra_ignore: &[String], out: &mut Vec<TemplateFile>) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("không đọc được thư mục {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if spec::is_ignored(&rel, extra_ignore) {
            continue;
        }
        // Symlinks are not followed: a template is data from the network, and
        // a symlink in it either escapes the destination or breaks on copy.
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk(root, &path, extra_ignore, out)?;
        } else if ft.is_file() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("không đọc được {}", path.display()))?;
            out.push(TemplateFile {
                rel,
                bytes,
                executable: is_executable(&path),
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    // Windows has no exec bit; the convention the templates rely on is that a
    // shell script is executable, which is what the unix side would have said.
    path.extension().map(|e| e == "sh").unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn loads_a_directory_template_and_skips_build_output() {
        let td = TempDir::new().unwrap();
        let t = td.path().join("app-x");
        write(&t, "template.json", r#"{"name":"app-x","kind":"app"}"#);
        write(&t, "senclaw-manifest.json", "{}");
        write(&t, "src/main.rs", "fn main() {}");
        write(&t, "node_modules/dep/index.js", "junk");
        write(&t, "target/debug/x", "junk");

        let loaded = load_from_dir(&t, "app-x", "test".into()).unwrap();
        let rels: Vec<&str> = loaded.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["senclaw-manifest.json", "src/main.rs"]);
    }

    #[test]
    fn a_files_subdir_becomes_the_payload_root() {
        let td = TempDir::new().unwrap();
        let t = td.path().join("skill-x");
        write(&t, "template.json", r#"{"name":"skill-x","kind":"skill"}"#);
        write(&t, "README.md", "docs about the template itself");
        write(&t, "files/SKILL.md", "the skill");

        let loaded = load_from_dir(&t, "skill-x", "test".into()).unwrap();
        let rels: Vec<&str> = loaded.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["SKILL.md"], "the template's own README is not payload");
    }

    #[test]
    fn kind_is_inferred_when_template_json_is_absent() {
        let td = TempDir::new().unwrap();
        let t = td.path().join("bare");
        write(&t, "SKILL.md", "---\nname: x\n---\n");
        let loaded = load_from_dir(&t, "bare", "test".into()).unwrap();
        assert_eq!(loaded.spec.kind, Some(spec::Kind::Skill));
    }

    #[test]
    fn both_repo_layouts_resolve() {
        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("templates/app-go")).unwrap();
        assert!(find_in_repo(td.path(), "app-go").is_some());

        let flat = TempDir::new().unwrap();
        std::fs::create_dir_all(flat.path().join("app-go")).unwrap();
        assert!(find_in_repo(flat.path(), "app-go").is_some());
        assert!(find_in_repo(flat.path(), "app-rust").is_none());
    }

    #[test]
    fn listing_a_repo_skips_non_templates() {
        let td = TempDir::new().unwrap();
        write(td.path(), "templates/app-go/template.json", "{}");
        write(td.path(), "templates/skill/SKILL.md", "x");
        std::fs::create_dir_all(td.path().join("templates/.github")).unwrap();
        std::fs::create_dir_all(td.path().join("templates/notes")).unwrap();

        let names: Vec<String> = list_repo(td.path()).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["app-go", "skill"]);
    }

    #[test]
    fn an_empty_template_is_an_error_not_an_empty_project() {
        let td = TempDir::new().unwrap();
        let t = td.path().join("empty");
        write(&t, "template.json", r#"{"name":"empty"}"#);
        assert!(load_from_dir(&t, "empty", "test".into()).is_err());
    }
}

//! A kit as a **zip bundle** rather than a lone JSON manifest.
//!
//! A JSON manifest can only carry what fits in a JSON string: a skill is one
//! `content` blob, a workflow one more, and an app cannot travel at all. A
//! bundle lifts that ceiling — the manifest still declares what the kit *is*,
//! while the files beside it carry what the manifest could only have inlined:
//!
//! ```text
//! kit.zip
//! ├── kit.json            ← the same manifest `/api/kits/install` already takes
//! ├── skills/<name>/…     ← a whole skill directory, scripts and references included
//! ├── workflows/<name>.md ← the workflow file verbatim
//! └── apps/<id>.zip       ← a Space App zip, installed through the app installer
//! ```
//!
//! Everything here is *additive*: a manifest that inlines its skills still
//! installs exactly as before, and a bundle may declare an item in the manifest,
//! on disk, or both. On both, the file wins — it is the richer copy, and an
//! author who shipped a directory meant the directory.
//!
//! Nothing in this module writes to disk. It turns bytes into a [`KitBundle`]
//! that [`super::installer`] then installs under the same never-overwrite rule
//! as every other kit item.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use super::manifest::{KitManifest, KitManifestError};

/// Manifest filenames accepted at the bundle root, in priority order.
pub const KIT_MANIFEST_NAMES: &[&str] = &["kit.json", "senclaw-kit.json", "manifest.json"];

/// Matches the Space App installer's cap — a kit bundle is the same kind of
/// artifact and the app inside it is bounded by that limit anyway.
pub const MAX_BUNDLE_BYTES: usize = 50 * 1024 * 1024;

/// Uncompressed ceiling. A 50 MB zip of zeros expands to gigabytes, and the
/// whole bundle is held in memory while it is read.
pub const MAX_UNPACKED_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum KitBundleError {
    #[error("bundle is {0} bytes; the limit is {MAX_BUNDLE_BYTES}")]
    TooLarge(usize),
    #[error("not a readable zip: {0}")]
    NotAZip(String),
    #[error(
        "no kit manifest in the bundle — expected one of {} at the root",
        KIT_MANIFEST_NAMES.join(", ")
    )]
    NoManifest,
    #[error("{0} is not valid JSON: {1}")]
    ManifestNotJson(String, String),
    #[error(transparent)]
    Manifest(#[from] KitManifestError),
    #[error("bundle unpacks to more than {MAX_UNPACKED_BYTES} bytes")]
    TooBigUnpacked,
}

/// One file inside a skill directory, path relative to that directory.
#[derive(Debug, Clone)]
pub struct BundleFile {
    pub rel: String,
    pub bytes: Vec<u8>,
}

/// A Space App riding along inside the kit, still zipped: the app installer
/// takes a zip, so unpacking here only to repack would be wasted work.
#[derive(Debug, Clone)]
pub struct BundleApp {
    /// Taken from the filename (`apps/<id>.zip`). The app's own manifest is
    /// authoritative for the real id; this is what the preview shows and what
    /// names the item if the zip turns out to be unreadable.
    pub id: String,
    pub zip: Vec<u8>,
}

/// A parsed bundle: the manifest plus everything the manifest could not inline.
#[derive(Debug, Clone)]
pub struct KitBundle {
    pub manifest: KitManifest,
    /// skill name → the files of its directory.
    pub skills: BTreeMap<String, Vec<BundleFile>>,
    /// workflow name → file contents.
    pub workflows: BTreeMap<String, String>,
    pub apps: Vec<BundleApp>,
}

impl KitBundle {
    /// A manifest with no bundle around it — the JSON-only install path,
    /// expressed as a bundle so the installer has one code path.
    pub fn from_manifest(manifest: KitManifest) -> Self {
        Self {
            manifest,
            skills: BTreeMap::new(),
            workflows: BTreeMap::new(),
            apps: Vec::new(),
        }
    }

    pub fn has_files(&self) -> bool {
        !self.skills.is_empty() || !self.workflows.is_empty() || !self.apps.is_empty()
    }

    /// Read a zip into a bundle.
    pub fn from_zip(bytes: &[u8]) -> Result<Self, KitBundleError> {
        if bytes.len() > MAX_BUNDLE_BYTES {
            return Err(KitBundleError::TooLarge(bytes.len()));
        }
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| KitBundleError::NotAZip(e.to_string()))?;

        // A zip made by "compress this folder" (or downloaded from GitHub) wraps
        // everything in one top-level directory. Strip it, or nothing below
        // matches the expected layout and the bundle reads as empty.
        let prefix = common_prefix(&mut archive);

        let mut manifest_raw: Option<(String, Vec<u8>)> = None;
        let mut skills: BTreeMap<String, Vec<BundleFile>> = BTreeMap::new();
        let mut workflows: BTreeMap<String, String> = BTreeMap::new();
        let mut apps: Vec<BundleApp> = Vec::new();
        let mut unpacked: u64 = 0;

        for i in 0..archive.len() {
            let mut file = match archive.by_index(i) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("[kits] bundle entry {i} unreadable: {e}");
                    continue;
                }
            };
            if file.is_dir() {
                continue;
            }
            // `enclosed_name` is the zip-slip guard: it returns None for any
            // path that escapes the archive root (`..`, absolute, drive-letter).
            let Some(path) = file.enclosed_name() else {
                tracing::warn!(
                    "[kits] bundle entry {} escapes the root — dropped",
                    file.name()
                );
                continue;
            };
            let rel = path.to_string_lossy().replace('\\', "/");
            let Some(rel) = strip_prefix(&rel, prefix.as_deref()) else {
                continue;
            };
            // Editor and OS droppings; a `__MACOSX/` sidecar would otherwise
            // register as a skill directory of its own.
            if rel.starts_with("__MACOSX/") || rel.ends_with("/.DS_Store") || rel == ".DS_Store" {
                continue;
            }

            unpacked = unpacked.saturating_add(file.size());
            if unpacked > MAX_UNPACKED_BYTES {
                return Err(KitBundleError::TooBigUnpacked);
            }

            let mut buf = Vec::new();
            if let Err(e) = file.read_to_end(&mut buf) {
                tracing::warn!("[kits] cannot read {rel} from bundle: {e}");
                continue;
            }

            if manifest_raw.is_none() && KIT_MANIFEST_NAMES.contains(&rel.as_str()) {
                manifest_raw = Some((rel.clone(), buf));
                continue;
            }
            if let Some(rest) = rel.strip_prefix("skills/") {
                // `skills/<name>/<path…>` — a bare file directly under skills/
                // has no directory to name the skill, so it is not one.
                if let Some((name, inner)) = rest.split_once('/') {
                    if !name.is_empty() && !inner.is_empty() {
                        skills
                            .entry(name.to_string())
                            .or_default()
                            .push(BundleFile {
                                rel: inner.to_string(),
                                bytes: buf,
                            });
                    }
                }
                continue;
            }
            if let Some(rest) = rel.strip_prefix("workflows/") {
                if let Some(name) = rest.strip_suffix(".md") {
                    if !name.is_empty() && !name.contains('/') {
                        workflows
                            .insert(name.to_string(), String::from_utf8_lossy(&buf).into_owned());
                    }
                }
                continue;
            }
            if let Some(rest) = rel.strip_prefix("apps/") {
                if let Some(id) = rest.strip_suffix(".zip") {
                    if !id.is_empty() && !id.contains('/') {
                        apps.push(BundleApp {
                            id: id.to_string(),
                            zip: buf,
                        });
                    }
                }
                continue;
            }
        }

        let (name, raw) = manifest_raw.ok_or(KitBundleError::NoManifest)?;
        let value: serde_json::Value = serde_json::from_slice(&raw)
            .map_err(|e| KitBundleError::ManifestNotJson(name, e.to_string()))?;
        // Same wrapper tolerance as the HTTP layer: a hand-written kit.json may
        // well wrap the manifest, and a 400 over a wrapper is miserable to debug.
        let inner = value
            .get("manifest")
            .filter(|v| v.is_object())
            .or_else(|| value.get("kit").filter(|v| v.is_object()))
            .unwrap_or(&value);
        // A bundle's items may live entirely in `skills/`, `workflows/` and
        // `apps/`, so an item-less manifest is legitimate here — emptiness is
        // judged below against the bundle as a whole, not the manifest alone.
        let manifest = KitManifest::parse_allowing_empty(inner)?;

        // A skill directory with no SKILL.md is not a skill — most likely a
        // stray folder. Dropping it here keeps the preview honest about what
        // will actually be installed.
        skills.retain(|name, files| {
            let has_entry = files.iter().any(|f| f.rel.eq_ignore_ascii_case("SKILL.md"));
            if !has_entry {
                tracing::warn!("[kits] skills/{name}/ has no SKILL.md — not installed");
            }
            has_entry
        });

        let bundle = Self {
            manifest,
            skills,
            workflows,
            apps,
        };
        // Same rule the JSON path applies, moved to where the whole picture is
        // known: a bundle that would install nothing is a mistake worth naming
        // rather than an install that reports zero items.
        if bundle.manifest.item_count() == 0 && !bundle.has_files() {
            return Err(KitBundleError::Manifest(KitManifestError::Empty(
                bundle.manifest.id,
            )));
        }
        Ok(bundle)
    }
}

/// The single top-level directory every entry shares, if there is one.
fn common_prefix<R: Read + std::io::Seek>(archive: &mut zip::ZipArchive<R>) -> Option<String> {
    let mut prefix: Option<String> = None;
    for i in 0..archive.len() {
        let Ok(file) = archive.by_index(i) else {
            continue;
        };
        let Some(path) = file.enclosed_name() else {
            continue;
        };
        let rel = path.to_string_lossy().replace('\\', "/");
        if rel.starts_with("__MACOSX/") {
            continue;
        }
        let top = rel.split('/').next().unwrap_or_default().to_string();
        // A file at the root means there is no wrapping directory at all.
        if top.is_empty() || !rel.contains('/') {
            return None;
        }
        match &prefix {
            None => prefix = Some(top),
            Some(p) if *p != top => return None,
            _ => {}
        }
    }
    prefix
}

fn strip_prefix(rel: &str, prefix: Option<&str>) -> Option<String> {
    match prefix {
        None => Some(rel.to_string()),
        Some(p) => rel
            .strip_prefix(p)
            .and_then(|r| r.strip_prefix('/'))
            .map(str::to_string)
            .filter(|r| !r.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            for (name, body) in entries {
                w.start_file(*name, SimpleFileOptions::default()).unwrap();
                w.write_all(body).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    const MANIFEST: &[u8] = br#"{"manifest":2,"id":"k","name":"K","version":"1.0.0"}"#;

    /// Declares one item of its own, so the bundle is non-empty regardless of
    /// what the files around it turn out to contribute.
    const MANIFEST_WITH_AGENT: &[u8] = br#"{"manifest":2,"id":"k","name":"K","version":"1.0.0",
        "agents":[{"name":"A","systemPrompt":"p"}]}"#;

    #[test]
    fn reads_manifest_skills_workflows_and_apps() {
        let zip = zip_of(&[
            ("kit.json", MANIFEST),
            ("skills/report/SKILL.md", b"# report"),
            ("skills/report/scripts/run.sh", b"echo hi"),
            ("workflows/morning.md", b"---\nname: morning\n---\n"),
            ("apps/dash.zip", b"PK-not-really"),
        ]);

        let bundle = KitBundle::from_zip(&zip).unwrap();

        assert_eq!(bundle.manifest.id, "k");
        assert_eq!(bundle.skills["report"].len(), 2, "whole directory travels");
        assert_eq!(bundle.workflows["morning"], "---\nname: morning\n---\n");
        assert_eq!(bundle.apps.len(), 1);
        assert_eq!(bundle.apps[0].id, "dash");
    }

    #[test]
    fn strips_a_single_wrapping_directory() {
        // What "compress this folder" and GitHub's zip download both produce.
        let zip = zip_of(&[
            ("my-kit/kit.json", MANIFEST),
            ("my-kit/skills/a/SKILL.md", b"# a"),
        ]);

        let bundle = KitBundle::from_zip(&zip).unwrap();

        assert_eq!(bundle.manifest.id, "k");
        assert!(
            bundle.skills.contains_key("a"),
            "prefix must not hide the skill"
        );
    }

    #[test]
    fn a_root_level_manifest_prevents_prefix_stripping() {
        // Two top-level names → no common wrapper to strip.
        let zip = zip_of(&[("kit.json", MANIFEST), ("skills/a/SKILL.md", b"# a")]);
        let bundle = KitBundle::from_zip(&zip).unwrap();
        assert!(bundle.skills.contains_key("a"));
    }

    #[test]
    fn skill_directory_without_skill_md_is_dropped() {
        let zip = zip_of(&[
            ("kit.json", MANIFEST_WITH_AGENT),
            ("skills/ghost/notes.txt", b"nothing here"),
        ]);

        let bundle = KitBundle::from_zip(&zip).unwrap();

        // Installing it would create a skill directory the loader ignores.
        assert!(bundle.skills.is_empty());
    }

    #[test]
    fn macos_sidecar_is_not_a_skill() {
        let zip = zip_of(&[
            ("kit.json", MANIFEST_WITH_AGENT),
            ("__MACOSX/._kit.json", b"junk"),
            ("skills/.DS_Store", b"junk"),
        ]);

        let bundle = KitBundle::from_zip(&zip).unwrap();

        assert!(bundle.skills.is_empty());
    }

    #[test]
    fn missing_manifest_is_named_as_such() {
        let zip = zip_of(&[("skills/a/SKILL.md", b"# a")]);
        let err = KitBundle::from_zip(&zip).unwrap_err();
        assert!(matches!(err, KitBundleError::NoManifest), "got {err}");
    }

    #[test]
    fn a_wrapper_key_in_kit_json_is_accepted() {
        let zip = zip_of(&[(
            "kit.json",
            br#"{"kit":{"manifest":2,"id":"wrapped","version":"1.0.0",
                "agents":[{"name":"A","systemPrompt":"p"}]}}"#,
        )]);
        let bundle = KitBundle::from_zip(&zip).unwrap();
        assert_eq!(bundle.manifest.id, "wrapped");
    }

    #[test]
    fn a_manifest_declaring_nothing_is_fine_when_the_files_carry_the_kit() {
        // The reason `parse_allowing_empty` exists: kit.json names the kit,
        // the skills directory *is* the kit.
        let zip = zip_of(&[("kit.json", MANIFEST), ("skills/a/SKILL.md", b"# a")]);

        let bundle = KitBundle::from_zip(&zip).unwrap();

        assert_eq!(bundle.manifest.item_count(), 0);
        assert!(bundle.has_files());
    }

    #[test]
    fn a_bundle_that_would_install_nothing_is_refused() {
        let zip = zip_of(&[("kit.json", MANIFEST)]);
        let err = KitBundle::from_zip(&zip).unwrap_err();
        assert!(
            matches!(err, KitBundleError::Manifest(KitManifestError::Empty(_))),
            "got {err}"
        );
    }

    #[test]
    fn garbage_is_reported_as_not_a_zip() {
        let err = KitBundle::from_zip(b"this is not a zip at all").unwrap_err();
        assert!(matches!(err, KitBundleError::NotAZip(_)), "got {err}");
    }

    #[test]
    fn oversized_bundle_is_refused_before_parsing() {
        let huge = vec![0u8; MAX_BUNDLE_BYTES + 1];
        let err = KitBundle::from_zip(&huge).unwrap_err();
        assert!(matches!(err, KitBundleError::TooLarge(_)), "got {err}");
    }
}

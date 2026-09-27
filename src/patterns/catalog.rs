//! What the "add a source" screen offers before the user types anything.
//!
//! Two kinds of entry, and the distinction is the point:
//!
//! * **Bundled** — patterns compiled into the binary, installed into a local
//!   source with no network at all. This is what SenClaw itself ships, and it
//!   is why a fresh install is not an empty screen.
//! * **Git preset** — a known library with its URL, pinned ref and subdirectory
//!   already filled in. Fabric's patterns live in `data/patterns` and its
//!   strategies in `data/strategies`; nobody should have to know that.
//!
//! Without this the dialog is five blank fields, and the only way to fill them
//! correctly is to have read someone else's repository layout first.

use serde::Serialize;

use super::store::{PatternStore, StoreError};
use super::{PatternSource, SourceKind};

/// Source id the bundled starter patterns install into.
pub const STARTER_SOURCE_ID: &str = "senclaw";

/// One pattern compiled into the binary by `build.rs`.
pub struct BundledPattern {
    pub name: &'static str,
    pub system: &'static str,
    pub user: Option<&'static str>,
}

// `BUNDLED_PATTERNS` (261 entries) and `BUNDLED_STRATEGIES` (9), walked out of
// `assets/patterns` and `assets/strategies` at compile time. Generated rather
// than hand-listed because re-vendoring Fabric replaces the tree wholesale —
// a list would be wrong within one update, and wrong in the silent direction.
include!(concat!(env!("OUT_DIR"), "/bundled_patterns.rs"));

/// Everything SenClaw ships: Fabric's library (MIT, vendored at a pinned tag)
/// plus SenClaw's own Vietnamese-first patterns.
///
/// Vendored rather than cloned on demand so a fresh install has a working
/// library **offline, on first launch**, with no network and no repository
/// that has to still exist. Provenance and how to re-vendor:
/// `assets/patterns/NOTICE.md`.
pub fn bundled_patterns() -> &'static [BundledPattern] {
    BUNDLED_PATTERNS
}

pub fn bundled_strategies() -> &'static [(&'static str, &'static str)] {
    BUNDLED_STRATEGIES
}

/// Tag the vendored Fabric copy was taken at. Kept next to the data it
/// describes so the catalog card cannot claim a version the files are not.
pub const VENDORED_FABRIC_TAG: &str = "v1.4.470";

/// A git library the UI offers as one tap.
pub struct GitPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub url: &'static str,
    /// Pinned on purpose — see [`super::source`].
    pub git_ref: &'static str,
    pub subdir: &'static str,
    pub strategies_subdir: Option<&'static str>,
    /// Roughly how many patterns to expect, for the card. Not enforced.
    pub approx_count: u32,
    pub license: &'static str,
}

pub static GIT_PRESETS: &[GitPreset] = &[
    // SenClaw's own repository, pointed at the same `assets/patterns` the
    // bundled copy was built from. This is the update path for the bundled
    // set: it tracks whatever SenClaw has vendored, so it moves when SenClaw
    // does rather than when Fabric does.
    GitPreset {
        id: "senclaw-git",
        name: "SenClaw (git)",
        description: "Cùng thư viện đi kèm bản cài, nhưng lấy thẳng từ repo SenClaw — dùng khi muốn bản mới hơn binary đang chạy.",
        url: "https://github.com/SenClaw/senclaw",
        git_ref: "main",
        subdir: "assets/patterns",
        strategies_subdir: Some("assets/strategies"),
        approx_count: 261,
        license: "MIT",
    },
    GitPreset {
        id: "fabric",
        name: "Fabric (upstream)",
        description: "Nguồn gốc của phần lớn thư viện đi kèm. Đăng ký nguồn này khi muốn theo bản mới của Fabric thay vì bản SenClaw đã đóng gói.",
        url: "https://github.com/danielmiessler/fabric",
        git_ref: VENDORED_FABRIC_TAG,
        subdir: "data/patterns",
        strategies_subdir: Some("data/strategies"),
        approx_count: 255,
        license: "MIT",
    },
];

/// A catalog row as the UI renders it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// `"bundled"` installs offline; `"git"` clones.
    pub kind: &'static str,
    pub count: u32,
    pub license: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdir: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategies_subdir: Option<&'static str>,
    /// Already present in this daemon's ledger — the card shows "installed"
    /// instead of offering the same thing twice.
    pub installed: bool,
    /// False when the ref is a moving branch. The UI says so, because a
    /// pattern becomes a system prompt.
    pub pinned: bool,
}

/// Every offer, marked with whether this daemon already has it.
pub fn entries(store: &PatternStore) -> Vec<CatalogEntry> {
    let have = |id: &str| store.source(id).is_ok();
    let mut out = vec![CatalogEntry {
        id: STARTER_SOURCE_ID,
        name: "Thư viện đi kèm",
        description: "Toàn bộ thư viện Fabric (MIT, ghim tag) cộng 6 pattern SenClaw viết cho tiếng Việt: tóm tắt, trích ý chính, viết lại cho gọn, phân tích log, biên bản họp, soạn email. Nằm sẵn trong bản cài — không cần mạng.",
        kind: "bundled",
        count: BUNDLED_PATTERNS.len() as u32,
        license: "MIT",
        url: None,
        git_ref: None,
        subdir: None,
        strategies_subdir: None,
        installed: have(STARTER_SOURCE_ID),
        // Compiled in, so it moves only when SenClaw itself is updated.
        pinned: true,
    }];

    out.extend(GIT_PRESETS.iter().map(|p| CatalogEntry {
        id: p.id,
        name: p.name,
        description: p.description,
        kind: "git",
        count: p.approx_count,
        license: p.license,
        url: Some(p.url),
        git_ref: Some(p.git_ref),
        subdir: Some(p.subdir),
        strategies_subdir: p.strategies_subdir,
        installed: have(p.id),
        pinned: super::source::looks_pinned(p.git_ref),
    }));
    out
}

/// Write the bundled patterns into their own local source.
///
/// A local source, not `user`: uninstalling is then a directory delete that
/// cannot take a hand-written pattern with it, and a starter pattern never
/// silently outranks one the user wrote — `user` is still resolved first.
///
/// Existing names are skipped rather than overwritten, so re-running this after
/// the user edited a starter pattern does not undo the edit.
pub fn install_starters(store: &PatternStore) -> Result<Vec<String>, StoreError> {
    let src = PatternSource {
        id: STARTER_SOURCE_ID.to_string(),
        name: "SenClaw".to_string(),
        kind: SourceKind::Local,
        url: None,
        git_ref: "main".to_string(),
        subdir: String::new(),
        strategies_subdir: None,
        enabled: true,
        installed_by: Some("builtin".to_string()),
        last_synced_at: None,
        last_error: None,
    };
    store.upsert_source(src.clone())?;

    let mut written = Vec::new();
    for p in BUNDLED_PATTERNS {
        match store.write(&src, p.name, p.system, p.user, false) {
            Ok(files) => written.push(files.name),
            Err(StoreError::Exists(_)) => {}
            Err(e) => tracing::warn!("[patterns] bundled \"{}\" skipped: {e}", p.name),
        }
    }

    // Strategies are global, not per-source, and a name that already exists is
    // left alone — one `cot` is the useful outcome, not one per library.
    let dir = store.strategies_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        for (name, body) in BUNDLED_STRATEGIES {
            let path = dir.join(format!("{name}.json"));
            if !path.exists() {
                let _ = std::fs::write(&path, body);
            }
        }
    }

    written.sort();
    Ok(written)
}

/// Look up a git preset by id.
pub fn git_preset(id: &str) -> Option<&'static GitPreset> {
    GIT_PRESETS.iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::PatternRegistry;

    /// Patterns SenClaw wrote itself, which must follow the full Fabric shape.
    const OURS: &[&str] = &[
        "tom_tat",
        "trich_y_chinh",
        "viet_lai_gon",
        "phan_tich_log",
        "bien_ban_hop",
        "soan_email",
    ];

    #[test]
    fn every_bundled_pattern_has_a_usable_name_and_a_body() {
        assert!(
            BUNDLED_PATTERNS.len() > 200,
            "the vendored library looks truncated: {} entries",
            BUNDLED_PATTERNS.len()
        );

        let mut seen = std::collections::BTreeSet::new();
        for p in BUNDLED_PATTERNS {
            // The directory name becomes the on-disk id, so it has to survive
            // sanitising unchanged or the catalog and the store disagree.
            assert_eq!(
                super::super::sanitize_name(p.name).unwrap(),
                p.name,
                "{} is not a usable directory id",
                p.name
            );
            assert!(!p.system.trim().is_empty(), "{} has an empty body", p.name);
            assert!(seen.insert(p.name), "{} is bundled twice", p.name);
        }

        // Both halves of the library actually made it in.
        for name in OURS {
            assert!(seen.contains(name), "SenClaw's own {name} is missing");
        }
        assert!(seen.contains("summarize"), "Fabric's summarize is missing");
        assert!(seen.contains("extract_wisdom"), "Fabric's extract_wisdom is missing");
    }

    /// Only our own patterns are held to the full convention. Fabric's 255 are
    /// vendored verbatim and a handful end differently — rewriting them to
    /// satisfy a test here would be edited-upstream-content, and the next
    /// re-vendor would silently undo it.
    #[test]
    fn senclaw_authored_patterns_follow_the_full_fabric_shape() {
        for name in OURS {
            let p = BUNDLED_PATTERNS.iter().find(|p| p.name == *name).unwrap();
            assert!(
                p.system.contains("# IDENTITY and PURPOSE"),
                "{name} is missing the identity header"
            );
            assert!(
                p.system.contains("# OUTPUT INSTRUCTIONS"),
                "{name} is missing the output instructions"
            );
            assert!(
                p.system.trim_end().ends_with("# INPUT:"),
                "{name} must end with the INPUT marker"
            );
        }
    }

    #[test]
    fn the_nine_strategies_are_bundled_and_parse() {
        assert_eq!(BUNDLED_STRATEGIES.len(), 9);
        for (name, body) in BUNDLED_STRATEGIES {
            serde_json::from_str::<crate::patterns::Strategy>(body)
                .unwrap_or_else(|e| panic!("strategy {name} does not parse: {e}"));
        }
        assert!(BUNDLED_STRATEGIES.iter().any(|(n, _)| *n == "cot"));
    }

    #[test]
    fn installing_starters_is_offline_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PatternStore::new(tmp.path());

        let first = install_starters(&store).unwrap();
        assert_eq!(first.len(), BUNDLED_PATTERNS.len());

        // Second run writes nothing new and destroys nothing.
        let second = install_starters(&store).unwrap();
        assert!(second.is_empty());
        let src = store.source(STARTER_SOURCE_ID).unwrap();
        assert_eq!(store.names_in(&src).len(), BUNDLED_PATTERNS.len());
        // Strategies ship with them; a library with no `cot` is half-installed.
        assert_eq!(
            crate::patterns::strategy::list_strategies(&store.strategies_dir()).len(),
            BUNDLED_STRATEGIES.len()
        );
    }

    #[test]
    fn a_user_edit_outranks_the_starter_of_the_same_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PatternStore::new(tmp.path());
        install_starters(&store).unwrap();

        let user = PatternSource::user();
        store
            .write(&user, "tom_tat", "# H\n\nBản của tôi.", None, false)
            .unwrap();

        let (src, files) = PatternRegistry::new(&store).resolve("tom_tat").unwrap();
        assert_eq!(src.id, crate::patterns::USER_SOURCE_ID);
        assert!(files.system.contains("Bản của tôi."));
    }

    #[test]
    fn the_catalog_marks_what_is_already_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = PatternStore::new(tmp.path());

        let before = entries(&store);
        assert!(before.iter().all(|e| !e.installed));
        // Fabric is offered with everything the dialog would otherwise ask for.
        let fabric = before.iter().find(|e| e.id == "fabric").unwrap();
        assert_eq!(fabric.subdir, Some("data/patterns"));
        assert_eq!(fabric.strategies_subdir, Some("data/strategies"));
        assert!(fabric.pinned, "the shipped preset must pin a tag");

        install_starters(&store).unwrap();
        let after = entries(&store);
        assert!(after.iter().find(|e| e.id == STARTER_SOURCE_ID).unwrap().installed);
        assert!(!after.iter().find(|e| e.id == "fabric").unwrap().installed);
    }
}

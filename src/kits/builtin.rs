//! Kits compiled into the binary.
//!
//! A built-in kit is offered by `/api/kits/available` alongside the
//! marketplace's, under the source id [`BUILTIN_SOURCE`], and installs through
//! the same path. It exists for one case: a kit whose whole payload is a
//! *reference* — a git URL and a pinned ref — where shipping the few hundred
//! bytes of manifest in the binary is strictly better than requiring a
//! marketplace to be configured before the feature works at all.
//!
//! It is **not** a place to bundle content. Anything with files in it belongs
//! in a marketplace source or a `.zip` bundle.

use serde::Serialize;

/// Source id reported for these, and the one `fetch_source_kit` matches on.
pub const BUILTIN_SOURCE: &str = "builtin";

/// One offered kit: the manifest text plus what the catalog row needs.
pub struct BuiltinKit {
    pub name: &'static str,
    pub id: &'static str,
    pub description: &'static str,
    pub version: &'static str,
    pub author: &'static str,
    pub homepage: &'static str,
    pub category: &'static str,
    /// The manifest itself, verbatim. Parsed on install, not at startup.
    pub manifest: &'static str,
}

/// Catalog row shape, matching what `kits_available` emits for a marketplace
/// kit so the UI needs no second code path.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinKitRow {
    pub source_id: &'static str,
    pub source_name: &'static str,
    pub name: &'static str,
    pub id: &'static str,
    pub description: &'static str,
    pub version: &'static str,
    pub author: &'static str,
    pub category: &'static str,
    pub homepage: &'static str,
    pub installable: bool,
}

pub static BUILTIN_KITS: &[BuiltinKit] = &[BuiltinKit {
    name: "Fabric Patterns",
    id: "fabric",
    description: "~250 prompt tác vụ của dự án Fabric (MIT) + 9 strategy suy luận. Tải về qua git khi cài.",
    version: "1.4.470",
    author: "Daniel Miessler and the Fabric contributors",
    homepage: "https://github.com/danielmiessler/fabric",
    category: "patterns",
    manifest: include_str!("../../assets/kits/fabric.json"),
}];

pub fn find(name: &str) -> Option<&'static BuiltinKit> {
    BUILTIN_KITS
        .iter()
        .find(|k| k.name == name || k.id == name)
}

/// Catalog rows for every built-in kit.
pub fn rows() -> Vec<BuiltinKitRow> {
    BUILTIN_KITS
        .iter()
        .map(|k| BuiltinKitRow {
            source_id: BUILTIN_SOURCE,
            source_name: "Đi kèm SenClaw",
            name: k.name,
            id: k.id,
            description: k.description,
            version: k.version,
            author: k.author,
            category: k.category,
            homepage: k.homepage,
            installable: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kits::KitManifest;

    /// Every bundled manifest must parse *now*, not when a user taps install.
    /// `include_str!` only proves the file exists.
    #[test]
    fn every_builtin_manifest_parses() {
        for kit in BUILTIN_KITS {
            let raw: serde_json::Value = serde_json::from_str(kit.manifest)
                .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", kit.name));
            let parsed = KitManifest::parse(&raw)
                .unwrap_or_else(|e| panic!("{} is not a usable kit: {e}", kit.name));
            // The catalog row and the manifest are two copies of the same
            // facts; a drift between them shows the wrong version in the UI.
            assert_eq!(parsed.id, kit.id, "{} id drift", kit.name);
            assert_eq!(parsed.version, kit.version, "{} version drift", kit.name);
        }
    }

    #[test]
    fn the_fabric_kit_pins_a_tag_and_points_at_the_pattern_subdir() {
        let raw: serde_json::Value = serde_json::from_str(find("fabric").unwrap().manifest).unwrap();
        let kit = KitManifest::parse(&raw).unwrap();
        let src = &kit.pattern_sources[0];

        assert_eq!(src.subdir, "data/patterns");
        assert_eq!(src.strategies_subdir.as_deref(), Some("data/strategies"));
        assert!(src.sync_on_install, "the point of the kit is one-tap import");
        // A pattern becomes a system prompt, so tracking `main` would let an
        // upstream commit rewrite instructions the agent obeys.
        assert!(
            src.git_ref.starts_with('v') && src.git_ref.contains('.'),
            "the shipped source must pin a tag, found {:?}",
            src.git_ref
        );
    }

    #[test]
    fn lookup_works_by_id_and_by_display_name() {
        assert!(find("fabric").is_some());
        assert!(find("Fabric Patterns").is_some());
        assert!(find("nope").is_none());
    }
}

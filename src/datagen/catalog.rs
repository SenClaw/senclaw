//! The dataset catalog: datasets written by people, ready to import in one
//! step. Two sorts of entry, the split Zen Patterns uses:
//! - **bundled** — compiled into the binary from `assets/datasets/`, so it
//!   installs offline. Only data SenClaw wrote or that is MIT / Apache /
//!   CC BY: the binary is MIT, and shipping share-alike data inside it would
//!   put the binary under that data's terms.
//! - **Hugging Face preset** — a public dataset, config, split and the
//!   commit it was checked at, with its column mapping written once here.
//!   Downloaded only when someone installs it.
//!
//! An entry whose terms need a decision — share-alike, non-commercial,
//! research-only, or a split that must not be trained on — installs only
//! with `acceptLicense: true`, which the UIs ask for with the entry's
//! `notice` in front of the person.
//!
//! The entries are `assets/datasets/catalog.json`; label maps are
//! `assets/datasets/label-maps/<id>.json`.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::mapping::{FieldSource, Kind, LabelMap};

const CATALOG_JSON: &str = include_str!("../../assets/datasets/catalog.json");

/// Bundled data files, by the name an entry's `source.bundled` uses.
const BUNDLED: &[(&str, &str)] = &[("senclaw-flows-vi.jsonl", include_str!("../../assets/datasets/senclaw-flows-vi.jsonl"))];

const LABEL_MAPS: &[(&str, &str)] =
    &[("massive-senclaw", include_str!("../../assets/datasets/label-maps/massive-senclaw.json"))];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub description: String,
    pub kind: Kind,
    pub lang: String,
    /// Rows in the source; what an install writes can be fewer (a label map
    /// drops what it does not map).
    pub rows: u64,
    /// SPDX-style id of the source's own license (not a mirror's).
    pub license: String,
    pub attribution: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    pub source: EntrySource,
    pub mapping: BTreeMap<String, FieldSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_map: Option<String>,
    #[serde(default)]
    pub flags: Flags,
    /// What a person should read before installing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// `seeds`, `eval`, `problems`, `corpus`.
    #[serde(default)]
    pub uses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum EntrySource {
    Bundled(String),
    Hf(HfPin),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HfPin {
    pub dataset: String,
    pub config: String,
    pub split: String,
    /// The commit the mapping was checked against.
    pub revision: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Flags {
    #[serde(default)]
    pub share_alike: bool,
    #[serde(default)]
    pub non_commercial: bool,
    #[serde(default)]
    pub research_only: bool,
    /// An evaluation split: measure with it, never train on it.
    #[serde(default)]
    pub no_training: bool,
}

impl Flags {
    pub fn needs_consent(&self) -> bool {
        self.share_alike || self.non_commercial || self.research_only || self.no_training
    }
}

pub fn entries() -> &'static [Entry] {
    static E: OnceLock<Vec<Entry>> = OnceLock::new();
    // Compiled in and checked by the tests below; unreadable here is a build
    // that should never have shipped.
    E.get_or_init(|| serde_json::from_str(CATALOG_JSON).expect("assets/datasets/catalog.json is valid"))
}

pub fn entry(id: &str) -> Option<&'static Entry> {
    entries().iter().find(|e| e.id == id)
}

pub fn bundled_text(file: &str) -> Option<&'static str> {
    BUNDLED.iter().find(|(f, _)| *f == file).map(|(_, t)| *t)
}

pub fn label_map(id: &str) -> Option<&'static LabelMap> {
    static M: OnceLock<Vec<(String, LabelMap)>> = OnceLock::new();
    M.get_or_init(|| {
        LABEL_MAPS
            .iter()
            .map(|(id, raw)| (id.to_string(), serde_json::from_str(raw).expect("bundled label map is valid")))
            .collect()
    })
    .iter()
    .find(|(k, _)| k == id)
    .map(|(_, m)| m)
}

/// The catalog for the UIs: each entry plus whether it is installed (a
/// finished dataset imported from it exists) and under which name.
pub fn listing(datasets: &[Value]) -> Value {
    let list: Vec<Value> = entries()
        .iter()
        .map(|e| {
            let installed = datasets.iter().find(|m| {
                m.get("state").and_then(Value::as_str) == Some("done") && catalog_id(m) == Some(e.id.as_str())
            });
            let mut v = serde_json::to_value(e).unwrap_or_else(|_| json!({}));
            v["needsConsent"] = json!(e.flags.needs_consent());
            v["bundled"] = json!(matches!(e.source, EntrySource::Bundled(_)));
            v["installed"] = json!(installed.is_some());
            v["dataset"] = installed.and_then(|m| m.get("name")).cloned().unwrap_or(Value::Null);
            v
        })
        .collect();
    json!(list)
}

/// The catalog entry a dataset manifest was imported from.
pub fn catalog_id(manifest: &Value) -> Option<&str> {
    manifest.pointer("/provenance/source/catalog").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datagen::mapping::check_mapping;
    use std::collections::BTreeSet;

    /// Skill names this repo bundles, plus `none`.
    fn known_labels() -> BTreeSet<String> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
        let mut out: BTreeSet<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().join("SKILL.md").is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        out.insert("none".into());
        out
    }

    #[test]
    fn every_entry_is_complete_and_its_mapping_fits_its_kind() {
        let mut ids = BTreeSet::new();
        for e in entries() {
            assert!(ids.insert(e.id.clone()), "duplicate id {}", e.id);
            assert!(!e.license.is_empty() && !e.attribution.is_empty(), "{}", e.id);
            check_mapping(e.kind, &e.mapping).unwrap_or_else(|err| panic!("{}: {err}", e.id));
            if let Some(lm) = &e.label_map {
                assert!(label_map(lm).is_some(), "{}: no label map {lm}", e.id);
            }
            match &e.source {
                EntrySource::Bundled(f) => {
                    assert!(bundled_text(f).is_some(), "{}: {f} is not bundled", e.id);
                    // Share-alike or restricted data is never compiled in.
                    assert!(!e.flags.needs_consent(), "{} is bundled but restricted", e.id);
                }
                EntrySource::Hf(pin) => {
                    assert!(crate::datagen::hf::valid_dataset_id(&pin.dataset), "{}", e.id);
                    assert_eq!(pin.revision.len(), 40, "{}: pin a full commit sha", e.id);
                }
            }
            if e.flags.needs_consent() {
                assert!(e.notice.is_some(), "{} needs a notice to consent to", e.id);
            }
        }
    }

    #[test]
    fn bundled_flows_and_the_massive_map_name_real_skills() {
        let known = known_labels();
        let text = bundled_text("senclaw-flows-vi.jsonl").unwrap();
        let mut per_label: BTreeMap<String, usize> = BTreeMap::new();
        for line in text.lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            let label = v["label"].as_str().unwrap().to_string();
            assert!(known.contains(&label), "senclaw-flows-vi uses unknown skill {label}");
            *per_label.entry(label).or_default() += 1;
        }
        assert_eq!(per_label.values().sum::<usize>() as u64, entry("senclaw-flows-vi").unwrap().rows);
        let lm = label_map("massive-senclaw").unwrap();
        for target in lm.map.values().flatten().chain(lm.accept.values().flatten()) {
            assert!(known.contains(target), "massive-senclaw maps to unknown skill {target}");
        }
        for source in lm.accept.keys() {
            assert!(matches!(lm.map.get(source), Some(Some(_))), "accept for unmapped {source}");
        }
    }

    #[test]
    fn listing_marks_installed_entries() {
        let datasets = vec![
            json!({"name": "massive-vi", "state": "done", "provenance": {"source": {"catalog": "massive-vi"}}}),
            json!({"name": "gsm8k", "state": "failed", "provenance": {"source": {"catalog": "gsm8k"}}}),
        ];
        let list = listing(&datasets);
        let get = |id: &str| list.as_array().unwrap().iter().find(|e| e["id"] == id).unwrap().clone();
        assert_eq!(get("massive-vi")["installed"], true);
        assert_eq!(get("massive-vi")["dataset"], "massive-vi");
        assert_eq!(get("massive-vi")["needsConsent"], true);
        assert_eq!(get("gsm8k")["installed"], false, "a failed install is not installed");
        assert_eq!(get("senclaw-flows-vi")["bundled"], true);
    }
}

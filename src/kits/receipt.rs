//! What a kit created, so it can be taken back out.
//!
//! Stored as one JSON document at `<kits_dir>/installed.json`. This is the
//! same ownership idea the Space App bundle uses (a marker file plus an
//! `<app_id>__` filename prefix), written down explicitly because a kit
//! touches things that live in four different places — persona files, skill
//! directories, workflow files, and rows in `background_tasks`.
//!
//! Only items the kit **created** are recorded. Anything it found already
//! present is skipped at install time and never enters the receipt, so
//! uninstalling a kit can't delete something the user made themselves.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Kind of thing a kit created. Serialised in the receipt, so these strings
/// are part of the on-disk format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KitItemKind {
    Agent,
    Skill,
    Workflow,
    Hook,
    Job,
    /// One pattern written into the kit's own pattern source.
    Pattern,
    /// A git pattern source the kit registered in `patterns/sources.json`.
    /// `engine_ref` is the source id, which is what removal needs — the
    /// checkout directory is the store's to decide, not the receipt's.
    #[serde(rename = "patternSource")]
    PatternSource,
    /// A Space App that shipped inside a zip bundle. `engine_ref` is the id the
    /// app installer registered it under.
    App,
}

impl KitItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Skill => "skill",
            Self::Workflow => "workflow",
            Self::Hook => "hook",
            Self::Job => "job",
            Self::Pattern => "pattern",
            Self::PatternSource => "patternSource",
            Self::App => "app",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitItemRecord {
    #[serde(rename = "type")]
    pub kind: KitItemKind,
    pub name: String,
    /// Absolute path for file-backed items (persona, skill dir, workflow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Engine id for database-backed items (background task id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitReceipt {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub name: String,
    /// What the kit is for. Kept in the receipt because it is the only place
    /// the installed-kits list can read it from — the manifest is gone by then,
    /// and a row showing an id and a version says nothing about what it does.
    /// Defaulted, so receipts written before this field still load.
    #[serde(default)]
    pub description: String,
    /// RFC3339 UTC.
    pub installed_at: String,
    #[serde(default)]
    pub items: Vec<KitItemRecord>,
    /// What the user answered when installing, so the UI can show a kit's
    /// settings after the fact.
    ///
    /// **Params marked `secret` are not in here.** This file is plain JSON in
    /// `~/.senclaw/kits`, and an API key belongs in it no more than in a log —
    /// the substituted value already lives wherever the kit put it, which is
    /// the user's choice, not an extra copy the ledger makes on its own.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
}

impl KitReceipt {
    pub fn items_of(&self, kind: KitItemKind) -> impl Iterator<Item = &KitItemRecord> {
        self.items.iter().filter(move |i| i.kind == kind)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ReceiptFile {
    #[serde(default)]
    kits: Vec<KitReceipt>,
}

/// Reader/writer for `<kits_dir>/installed.json`.
pub struct KitReceiptStore {
    path: PathBuf,
}

impl KitReceiptStore {
    pub fn new(kits_dir: &Path) -> Self {
        Self {
            path: kits_dir.join("installed.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A corrupt or half-written receipt file reads as "nothing installed"
    /// rather than failing every kit call: losing the ledger costs the
    /// uninstall button, while erroring out would take the whole kits API
    /// down with it. The problem is logged, not swallowed silently.
    pub fn list(&self) -> Vec<KitReceipt> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                tracing::warn!("[kits] cannot read {}: {e}", self.path.display());
                return Vec::new();
            }
        };
        match serde_json::from_str::<ReceiptFile>(&raw) {
            Ok(file) => file.kits,
            Err(e) => {
                tracing::warn!("[kits] {} is not valid JSON: {e}", self.path.display());
                Vec::new()
            }
        }
    }

    pub fn get(&self, kit_id: &str) -> Option<KitReceipt> {
        self.list().into_iter().find(|k| k.id == kit_id)
    }

    /// Insert or replace one kit's receipt, keeping the file sorted by id so
    /// diffs stay readable.
    pub fn save(&self, receipt: KitReceipt) -> Result<()> {
        let mut by_id: BTreeMap<String, KitReceipt> =
            self.list().into_iter().map(|k| (k.id.clone(), k)).collect();
        by_id.insert(receipt.id.clone(), receipt);
        self.write(by_id.into_values().collect())
    }

    pub fn remove(&self, kit_id: &str) -> Result<()> {
        let kept: Vec<KitReceipt> = self.list().into_iter().filter(|k| k.id != kit_id).collect();
        self.write(kept)
    }

    fn write(&self, kits: Vec<KitReceipt>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let body = serde_json::to_string_pretty(&ReceiptFile { kits })?;
        // Write beside the target then rename: a crash mid-write must not
        // leave a truncated ledger, because that is exactly the file needed to
        // clean up afterwards.
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename into {}", self.path.display()))?;
        Ok(())
    }
}

/// Now, as RFC3339 UTC.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, KitReceiptStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = KitReceiptStore::new(dir.path());
        (dir, store)
    }

    fn receipt(id: &str) -> KitReceipt {
        KitReceipt {
            id: id.into(),
            version: "1.0.0".into(),
            name: id.into(),
            description: String::new(),
            installed_at: now_rfc3339(),
            params: BTreeMap::new(),
            items: vec![KitItemRecord {
                kind: KitItemKind::Agent,
                name: "A".into(),
                path: Some("/tmp/a.md".into()),
                engine_ref: None,
            }],
        }
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let (_dir, store) = store();
        assert!(store.list().is_empty());
        assert!(store.get("nope").is_none());
    }

    #[test]
    fn saves_and_reads_back() {
        let (_dir, store) = store();
        store.save(receipt("a")).unwrap();
        store.save(receipt("b")).unwrap();

        let all = store.list();
        assert_eq!(all.len(), 2);
        let a = store.get("a").unwrap();
        assert_eq!(a.items.len(), 1);
        assert_eq!(a.items[0].kind, KitItemKind::Agent);
        assert_eq!(a.items[0].path.as_deref(), Some("/tmp/a.md"));
    }

    #[test]
    fn saving_the_same_kit_twice_replaces_it() {
        let (_dir, store) = store();
        store.save(receipt("a")).unwrap();
        let mut second = receipt("a");
        second.version = "2.0.0".into();
        store.save(second).unwrap();

        let all = store.list();
        assert_eq!(all.len(), 1, "one row per kit id");
        assert_eq!(all[0].version, "2.0.0");
    }

    #[test]
    fn remove_drops_only_that_kit() {
        let (_dir, store) = store();
        store.save(receipt("a")).unwrap();
        store.save(receipt("b")).unwrap();

        store.remove("a").unwrap();

        let ids: Vec<String> = store.list().into_iter().map(|k| k.id).collect();
        assert_eq!(ids, vec!["b"]);
    }

    #[test]
    fn corrupt_file_reads_as_empty_instead_of_erroring() {
        let (dir, store) = store();
        fs::write(dir.path().join("installed.json"), "{ not json").unwrap();

        // Losing the ledger costs the uninstall button; throwing here would
        // take the whole kits API down with it.
        assert!(store.list().is_empty());
        // …and it must still be writable afterwards.
        store.save(receipt("a")).unwrap();
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn items_of_filters_by_kind() {
        let mut r = receipt("a");
        r.items.push(KitItemRecord {
            kind: KitItemKind::Job,
            name: "J".into(),
            path: None,
            engine_ref: Some("bg-1".into()),
        });

        let jobs: Vec<&KitItemRecord> = r.items_of(KitItemKind::Job).collect();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].engine_ref.as_deref(), Some("bg-1"));
    }
}

//! Feeding a pipeline from a dataset already on disk: `seedsFrom` on
//! `expand_seeds` and `executable_qa`, `problemsFrom` on `reasoning_traces`.
//! The records are read when the job starts and written into its parameters,
//! so the manifest still shows exactly what the run was given.
//!
//! The pick is deterministic and spread across the file — evenly spaced
//! indices, per label when `perLabel` is set — so an imported dataset
//! sorted by label (most are) does not hand over only its first class.
//!
//! A chat may read its own datasets and any imported from the catalog: those
//! hold public data, and a chat asking to "use MASSIVE as seeds" should not
//! need it installed twice. Everything else stays with the chat that made it.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::store;

/// Most records read from a dataset to pick from.
const MAX_READ: usize = 200_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DatasetRef {
    pub dataset: String,
    /// Most records taken; default and ceiling: what the pipeline accepts.
    #[serde(default)]
    pub limit: Option<usize>,
    /// At most this many per label.
    #[serde(default)]
    pub per_label: Option<usize>,
}

/// May `owner` (a chat, or `None` for the daemon's own API) read this dataset?
pub fn readable_by(manifest: &Value, owner: Option<&str>) -> bool {
    owner.is_none_or(|o| manifest.get("owner").and_then(Value::as_str) == Some(o))
        || super::catalog::catalog_id(manifest).is_some()
}

/// The records `r` picks out of its dataset, at most `cap`. `writing` says
/// whether a job id is still writing its dataset.
pub fn pick(root: &Path, owner: Option<&str>, r: &DatasetRef, cap: usize, writing: impl Fn(&str) -> bool) -> Result<Vec<Value>, String> {
    let name = r.dataset.trim();
    let missing = || format!("no dataset {name}");
    let dir = store::dataset_dir(root, name).ok_or_else(missing)?;
    let manifest = store::read_manifest(&dir).ok_or_else(missing)?;
    if !readable_by(&manifest, owner) {
        return Err(missing());
    }
    if manifest.get("jobId").and_then(Value::as_str).is_some_and(&writing) {
        return Err(format!("dataset {name} is still being written — wait for its job to finish"));
    }
    let (records, _) = store::read_records(&dir, 0, MAX_READ, usize::MAX).map_err(|e| format!("cannot read dataset {name}: {e}"))?;
    let limit = r.limit.unwrap_or(cap).clamp(1, cap);
    let picked = match r.per_label {
        Some(n) => {
            let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
            for rec in records {
                let label = rec.get("label").and_then(Value::as_str).unwrap_or_default().to_string();
                groups.entry(label).or_default().push(rec);
            }
            groups.into_values().flat_map(|g| spread(g, n.max(1))).collect::<Vec<_>>()
        }
        None => records,
    };
    Ok(spread(picked, limit))
}

/// `n` items at evenly spaced positions, in order.
fn spread(items: Vec<Value>, n: usize) -> Vec<Value> {
    let len = items.len();
    if len <= n {
        return items;
    }
    let mut slots: Vec<Option<Value>> = items.into_iter().map(Some).collect();
    (0..n).filter_map(|i| slots[i * len / n].take()).collect()
}

/// Records converted to a pipeline's input type; the ones that do not fit
/// are skipped, and none fitting is an error naming what was needed.
pub fn convert<T: serde::de::DeserializeOwned>(records: Vec<Value>, dataset: &str, needs: &str) -> Result<Vec<T>, String> {
    let out: Vec<T> = records.into_iter().filter_map(|r| serde_json::from_value(r).ok()).collect();
    if out.is_empty() {
        return Err(format!("dataset {dataset} has no records with {needs}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datagen::store::DatasetWriter;
    use serde_json::json;

    fn dataset(root: &Path, name: &str, manifest: Value, rows: &[Value]) {
        let mut w = DatasetWriter::create(root, name).unwrap();
        for r in rows {
            w.append(r).unwrap();
        }
        store::write_manifest(w.dir(), &manifest).unwrap();
    }

    fn r(dataset: &str, limit: Option<usize>, per_label: Option<usize>) -> DatasetRef {
        DatasetRef { dataset: dataset.into(), limit, per_label }
    }

    #[test]
    fn picks_spread_across_the_file_and_per_label() {
        let tmp = tempfile::tempdir().unwrap();
        let rows: Vec<Value> = (0..10).map(|i| json!({"text": format!("t{i}"), "label": if i < 8 { "a" } else { "b" }})).collect();
        dataset(tmp.path(), "flows", json!({"owner": null}), &rows);
        let texts = |v: Vec<Value>| v.iter().map(|r| r["text"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(texts(pick(tmp.path(), None, &r("flows", Some(3), None), 100, |_| false).unwrap()), ["t0", "t3", "t6"]);
        assert_eq!(texts(pick(tmp.path(), None, &r("flows", None, Some(2)), 100, |_| false).unwrap()), ["t0", "t4", "t8", "t9"]);
        assert_eq!(pick(tmp.path(), None, &r("flows", Some(500), None), 4, |_| false).unwrap().len(), 4, "capped by the pipeline");
    }

    #[test]
    fn chats_read_their_own_and_catalog_datasets_only() {
        let tmp = tempfile::tempdir().unwrap();
        let row = [json!({"text": "x"})];
        dataset(tmp.path(), "mine", json!({"owner": "web:a"}), &row);
        dataset(tmp.path(), "massive-vi", json!({"owner": null, "provenance": {"source": {"catalog": "massive-vi"}}}), &row);
        dataset(tmp.path(), "theirs", json!({"owner": "web:b"}), &row);
        let ok = |name: &str| pick(tmp.path(), Some("web:a"), &r(name, None, None), 10, |_| false).is_ok();
        assert!(ok("mine") && ok("massive-vi"));
        assert!(!ok("theirs"));
        assert!(pick(tmp.path(), None, &r("theirs", None, None), 10, |_| false).is_ok(), "the daemon's API sees all");
        assert!(pick(tmp.path(), None, &r("../mine", None, None), 10, |_| false).is_err());
    }

    #[test]
    fn a_dataset_still_being_written_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        dataset(tmp.path(), "busy", json!({"jobId": "j1"}), &[json!({"text": "x"})]);
        let err = pick(tmp.path(), None, &r("busy", None, None), 10, |id| id == "j1").unwrap_err();
        assert!(err.contains("still being written"));
    }

    #[test]
    fn records_that_do_not_fit_are_skipped_and_none_fitting_says_why() {
        #[derive(Debug, serde::Deserialize)]
        struct NeedsText {
            #[allow(dead_code)]
            text: String,
        }
        let ok: Vec<NeedsText> = convert(vec![json!({"text": "a"}), json!({"question": "b"})], "d", "a text").unwrap();
        assert_eq!(ok.len(), 1);
        let err = convert::<NeedsText>(vec![json!({"question": "b"})], "d", "a text").unwrap_err();
        assert_eq!(err, "dataset d has no records with a text");
    }
}

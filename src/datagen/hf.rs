//! Reading a public Hugging Face dataset through the dataset viewer API
//! (`datasets-server.huggingface.co`): `/splits` to list what a dataset has,
//! `/rows` for pages of at most 100 rows, and `huggingface.co/api/datasets`
//! for the commit, the license tag and whether it is gated.
//!
//! The viewer serves the dataset as it is **now** — `/rows` takes no
//! revision. A catalog entry's pinned commit is therefore a check, not a
//! selector: the import records the commit it actually read, and says so
//! when that is not the pinned one, instead of claiming a reproducibility it
//! does not have. Reading an exact old commit would mean downloading and
//! decoding its parquet files.
//!
//! Gated and private datasets are refused: there is no Hugging Face token in
//! the daemon, and adding one is a secret to keep for one feature.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

pub const VIEWER_BASE: &str = "https://datasets-server.huggingface.co";
pub const HUB_BASE: &str = "https://huggingface.co";
/// Rows per `/rows` call; the API's own maximum.
pub const PAGE: u32 = 100;
/// Attempts per call. The viewer rate-limits a split of a few thousand rows
/// read at its 100-row page size; five tries (15 s of waiting) gave up on
/// MASSIVE-vi at row 2,400 while a second import ran beside it. Eight, with
/// the wait capped at 30 s, rides out about a minute and a half.
const TRIES: u32 = 8;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DatasetInfo {
    pub sha: Option<String>,
    /// From the card or a `license:` tag, as Hugging Face spells it.
    pub license: Option<String>,
    pub gated: bool,
    pub private: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Split {
    pub config: String,
    pub split: String,
}

#[derive(Debug, Clone, Default)]
pub struct RowsPage {
    /// `(row index, row)`; a row the viewer cut short is `None`.
    pub rows: Vec<(u64, Option<Value>)>,
    /// Top-level column names in the dataset's own order.
    pub columns: Vec<String>,
    pub total: u64,
    /// The viewer only converted the first part of a large split.
    pub partial: bool,
}

#[async_trait]
pub trait HfClient: Send + Sync {
    async fn info(&self, dataset: &str) -> Result<DatasetInfo, String>;
    async fn splits(&self, dataset: &str) -> Result<Vec<Split>, String>;
    async fn rows(&self, dataset: &str, config: &str, split: &str, offset: u64, length: u32) -> Result<RowsPage, String>;
}

/// `owner/name` or `name`, each part `[A-Za-z0-9._-]`, the way the Hub
/// spells repository ids. Anything else would be sent into a URL path.
pub fn valid_dataset_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('/').collect();
    (1..=2).contains(&parts.len())
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 96
                && !p.starts_with('.')
                && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
}

/// Config and split names (`xquad.vi`, `test`).
pub fn valid_part(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The public Hub, no credentials.
pub struct PublicHf {
    client: reqwest::Client,
    viewer: String,
    hub: String,
}

impl PublicHf {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("SenClaw-datagen/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        PublicHf { client, viewer: VIEWER_BASE.into(), hub: HUB_BASE.into() }
    }

    /// GET a JSON document, retrying a rate limit, a server error or a
    /// dropped connection; anything else is the answer.
    async fn get(&self, url: &str, query: &[(&str, String)]) -> Result<Value, String> {
        let mut wait = Duration::from_secs(1);
        let mut last = String::new();
        for attempt in 1..=TRIES {
            match self.client.get(url).query(query).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let body = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        return serde_json::from_str(&body).map_err(|e| format!("Hugging Face sent an unreadable answer: {e}"));
                    }
                    let detail = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_else(|| status.canonical_reason().unwrap_or("error").to_string());
                    if status.as_u16() != 429 && !status.is_server_error() {
                        return Err(match status.as_u16() {
                            401 | 403 | 404 => format!("Hugging Face has no public dataset there ({status}): {detail}"),
                            _ => format!("Hugging Face refused the request ({status}): {detail}"),
                        });
                    }
                    last = format!("Hugging Face answered {status}: {detail}");
                    if attempt < TRIES {
                        tokio::time::sleep(retry_after.unwrap_or(wait).min(MAX_RETRY_WAIT)).await;
                    }
                }
                Err(e) => {
                    last = format!("cannot reach Hugging Face: {e}");
                    if attempt < TRIES {
                        tokio::time::sleep(wait).await;
                    }
                }
            }
            wait = (wait * 2).min(MAX_RETRY_WAIT);
        }
        Err(format!("{last} (tried {TRIES} times)"))
    }
}

impl Default for PublicHf {
    fn default() -> Self {
        PublicHf::new()
    }
}

#[async_trait]
impl HfClient for PublicHf {
    async fn info(&self, dataset: &str) -> Result<DatasetInfo, String> {
        let v = self.get(&format!("{}/api/datasets/{dataset}", self.hub), &[]).await?;
        Ok(parse_info(&v))
    }

    async fn splits(&self, dataset: &str) -> Result<Vec<Split>, String> {
        let v = self.get(&format!("{}/splits", self.viewer), &[("dataset", dataset.to_string())]).await?;
        Ok(parse_splits(&v))
    }

    async fn rows(&self, dataset: &str, config: &str, split: &str, offset: u64, length: u32) -> Result<RowsPage, String> {
        let q = [
            ("dataset", dataset.to_string()),
            ("config", config.to_string()),
            ("split", split.to_string()),
            ("offset", offset.to_string()),
            ("length", length.min(PAGE).to_string()),
        ];
        let v = self.get(&format!("{}/rows", self.viewer), &q).await?;
        Ok(parse_rows(&v))
    }
}

fn parse_info(v: &Value) -> DatasetInfo {
    let card = v.get("cardData").and_then(|c| c.get("license"));
    let from_card = match card {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).next().map(str::to_string),
        _ => None,
    };
    let from_tags = v
        .get("tags")
        .and_then(Value::as_array)
        .and_then(|t| t.iter().filter_map(Value::as_str).find_map(|t| t.strip_prefix("license:").map(str::to_string)));
    DatasetInfo {
        sha: v.get("sha").and_then(Value::as_str).map(str::to_string),
        license: from_card.or(from_tags),
        // `false`, or the approval mode ("auto", "manual").
        gated: !matches!(v.get("gated"), None | Some(Value::Bool(false)) | Some(Value::Null)),
        private: v.get("private").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn parse_splits(v: &Value) -> Vec<Split> {
    v.get("splits")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    Some(Split {
                        config: s.get("config")?.as_str()?.to_string(),
                        split: s.get("split")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_rows(v: &Value) -> RowsPage {
    let rows = v
        .get("rows")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|r| {
                    let idx = r.get("row_idx").and_then(Value::as_u64).unwrap_or(0);
                    let cut = r.get("truncated_cells").and_then(Value::as_array).is_some_and(|c| !c.is_empty());
                    (idx, if cut { None } else { r.get("row").cloned() })
                })
                .collect()
        })
        .unwrap_or_default();
    let columns = v
        .get("features")
        .and_then(Value::as_array)
        .map(|f| f.iter().filter_map(|x| x.get("name").and_then(Value::as_str).map(str::to_string)).collect())
        .unwrap_or_default();
    RowsPage {
        rows,
        columns,
        total: v.get("num_rows_total").and_then(Value::as_u64).unwrap_or(0),
        partial: v.get("partial").and_then(Value::as_bool).unwrap_or(false),
    }
}

#[cfg(test)]
pub mod fake {
    //! A dataset served from memory, for tests that must not touch the network.
    use super::*;
    use std::sync::Mutex;

    pub struct FakeHf {
        pub info: DatasetInfo,
        pub splits: Vec<Split>,
        pub rows: Vec<Value>,
        pub partial: bool,
        /// `offset` of every `/rows` call, in order.
        pub calls: Mutex<Vec<u64>>,
    }

    impl FakeHf {
        pub fn new(rows: Vec<Value>) -> Self {
            FakeHf {
                info: DatasetInfo { sha: Some("abc123".into()), license: Some("mit".into()), gated: false, private: false },
                splits: vec![Split { config: "default".into(), split: "train".into() }],
                rows,
                partial: false,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl HfClient for FakeHf {
        async fn info(&self, _: &str) -> Result<DatasetInfo, String> {
            Ok(self.info.clone())
        }
        async fn splits(&self, _: &str) -> Result<Vec<Split>, String> {
            Ok(self.splits.clone())
        }
        async fn rows(&self, _: &str, _: &str, _: &str, offset: u64, length: u32) -> Result<RowsPage, String> {
            self.calls.lock().unwrap().push(offset);
            let rows = self
                .rows
                .iter()
                .enumerate()
                .skip(offset as usize)
                .take(length.min(PAGE) as usize)
                .map(|(i, r)| (i as u64, Some(r.clone())))
                .collect();
            let mut columns: Vec<String> = Vec::new();
            for r in &self.rows {
                for k in r.as_object().into_iter().flat_map(|o| o.keys()) {
                    if !columns.contains(k) {
                        columns.push(k.clone());
                    }
                }
            }
            Ok(RowsPage { rows, columns, total: self.rows.len() as u64, partial: self.partial })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dataset_ids_cannot_carry_a_path() {
        assert!(valid_dataset_id("mteb/amazon_massive_intent"));
        assert!(valid_dataset_id("gsm8k"));
        assert!(!valid_dataset_id("a/b/c"));
        assert!(!valid_dataset_id("../x"));
        assert!(!valid_dataset_id("a/"));
        assert!(!valid_dataset_id("a b"));
        assert!(valid_part("xquad.vi") && !valid_part("vi&x=1"));
    }

    #[test]
    fn info_reads_the_license_from_the_card_or_the_tags_and_spots_gating() {
        let i = parse_info(&json!({"sha": "940f", "gated": false, "private": false, "cardData": {"license": ["cc-by-sa-4.0"]}}));
        assert_eq!(i, DatasetInfo { sha: Some("940f".into()), license: Some("cc-by-sa-4.0".into()), gated: false, private: false });
        let i = parse_info(&json!({"gated": "manual", "tags": ["task:x", "license:mit"]}));
        assert!(i.gated);
        assert_eq!(i.license.as_deref(), Some("mit"));
    }

    #[test]
    fn rows_pages_keep_indices_and_drop_cut_rows() {
        let p = parse_rows(&json!({
            "features": [{"name": "text"}, {"name": "label"}],
            "rows": [
                {"row_idx": 7, "row": {"text": "a"}, "truncated_cells": []},
                {"row_idx": 8, "row": {"text": "b…"}, "truncated_cells": ["text"]}
            ],
            "num_rows_total": 2974, "partial": false
        }));
        assert_eq!(p.total, 2974);
        assert_eq!(p.columns, ["text", "label"]);
        assert_eq!(p.rows[0], (7, Some(json!({"text": "a"}))));
        assert_eq!(p.rows[1], (8, None));
        let s = parse_splits(&json!({"splits": [{"dataset": "d", "config": "vi", "split": "test"}]}));
        assert_eq!(s, vec![Split { config: "vi".into(), split: "test".into() }]);
    }
}

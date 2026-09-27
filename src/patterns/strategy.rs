//! Strategies — reasoning techniques bolted onto a pattern without editing it.
//!
//! A strategy is two fields, and that is the point: it separates *how to
//! think* from *what to do*, so one `cot.json` applies to every pattern
//! installed rather than being copy-pasted into each.
//!
//! ```json
//! { "description": "Chain-of-Thought (CoT) Prompting",
//!   "prompt": "Think step by step to answer the question. Return the final answer in the required format." }
//! ```
//!
//! The wire format is Fabric's, so `data/strategies/*.json` imports as-is.
//!
//! Not to be confused with `adaptive_thinking` in
//! [`crate::zen_core::query_llm`]: that decides the model's thinking *budget*,
//! this decides the *method* and is plain prompt text.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::store::sanitize_name;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Strategy {
    /// Human label for the picker.
    #[serde(default)]
    pub description: String,
    /// Text appended to the pattern's system prompt.
    pub prompt: String,
    /// Filled in by [`list_strategies`] from the file stem; not stored.
    #[serde(default, skip_deserializing)]
    pub name: String,
}

/// Every strategy in `<patterns_dir>/strategies`, sorted by name.
///
/// A file that does not parse is skipped rather than failing the listing: one
/// malformed JSON dropped into the folder must not empty the picker.
pub fn list_strategies(dir: &Path) -> Vec<Strategy> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Strategy> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .filter_map(|e| {
            let stem = e.path().file_stem()?.to_str()?.to_owned();
            let raw = fs::read_to_string(e.path()).ok()?;
            let mut s: Strategy = serde_json::from_str(&raw).ok()?;
            s.name = stem;
            Some(s)
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// One strategy by name, or `None` when absent or unparseable.
pub fn read_strategy(dir: &Path, name: &str) -> Option<Strategy> {
    let safe = sanitize_name(name).ok()?;
    let raw = fs::read_to_string(dir.join(format!("{safe}.json"))).ok()?;
    let mut s: Strategy = serde_json::from_str(&raw).ok()?;
    s.name = safe;
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fabric_shaped_strategies_and_skips_junk() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cot.json"),
            r#"{"description":"Chain-of-Thought (CoT) Prompting","prompt":"Think step by step."}"#,
        )
        .unwrap();
        // No `prompt` key — unusable, must not reach the picker.
        fs::write(dir.path().join("broken.json"), r#"{"description":"x"}"#).unwrap();
        fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let list = list_strategies(dir.path());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "cot");
        assert_eq!(list[0].prompt, "Think step by step.");

        assert!(read_strategy(dir.path(), "cot").is_some());
        assert!(read_strategy(dir.path(), "broken").is_none());
        // A traversal attempt resolves to a name that simply does not exist.
        assert!(read_strategy(dir.path(), "../../etc/passwd").is_none());
    }
}

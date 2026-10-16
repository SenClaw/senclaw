//! How a row of someone else's dataset becomes a record the pipelines read.
//!
//! An import names a **kind** — the record shape — and a **mapping** from
//! each of the kind's fields to a column of the source row. The kinds are
//! the shapes the pipelines already take, so an imported dataset feeds them
//! with no conversion:
//!
//! | kind | fields | read by |
//! |---|---|---|
//! | `labeled_text` | `text`, `label?` | `expand_seeds` seeds; a router eval |
//! | `qa_context` | `question`, `answer`, `context` | retrieval evals; `doc_qa`-style seeds |
//! | `problem_answer` | `problem`, `answer?` | `reasoning_traces`, `executable_qa` |
//! | `chat` | `user`, `assistant`, `system?` | fine-tune exports, like `export_logs` |
//!
//! A column is a path: `text`, `answers.text.0` (key, key, list index). A
//! field may also join several columns, or keep only what follows a marker
//! — GSM8K's answer is a worked solution ending `#### 18`.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    LabeledText,
    QaContext,
    ProblemAnswer,
    Chat,
}

/// Longest value kept in a field; a longer one is `too_long`. A passage of
/// context may be long, a request may not.
const MAX_FIELD_CHARS: usize = 20_000;
const MAX_CONTEXT_CHARS: usize = 60_000;

impl Kind {
    /// The kind's fields and whether each is required.
    pub fn fields(self) -> &'static [(&'static str, bool)] {
        match self {
            Kind::LabeledText => &[("text", true), ("label", false)],
            Kind::QaContext => &[("question", true), ("answer", true), ("context", true)],
            Kind::ProblemAnswer => &[("problem", true), ("answer", false)],
            Kind::Chat => &[("user", true), ("assistant", true), ("system", false)],
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::LabeledText => "labeled_text",
            Kind::QaContext => "qa_context",
            Kind::ProblemAnswer => "problem_answer",
            Kind::Chat => "chat",
        }
    }

    fn allows(self, field: &str) -> bool {
        field == "id" || self.fields().iter().any(|(f, _)| *f == field)
    }
}

/// Where one field comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldSource {
    Path(String),
    Spec(FieldSpec),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    /// One column, or several joined by a blank line (empty ones skipped).
    pub from: Paths,
    /// Keep only the text after the last occurrence of this marker; a value
    /// without it counts as missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Paths {
    One(String),
    Many(Vec<String>),
}

impl FieldSource {
    fn paths(&self) -> Vec<&str> {
        match self {
            FieldSource::Path(p) => vec![p.as_str()],
            FieldSource::Spec(FieldSpec { from: Paths::One(p), .. }) => vec![p.as_str()],
            FieldSource::Spec(FieldSpec { from: Paths::Many(ps), .. }) => ps.iter().map(String::as_str).collect(),
        }
    }

    /// The field's text in `row`, trimmed; `None` when absent or empty.
    pub fn extract(&self, row: &Value) -> Option<String> {
        let parts: Vec<String> = self.paths().into_iter().filter_map(|p| scalar_text(lookup(row, p)?)).collect();
        let joined = parts.join("\n\n");
        let text = match self {
            FieldSource::Spec(FieldSpec { after: Some(marker), .. }) => {
                let (_, tail) = joined.rsplit_once(marker.as_str())?;
                tail.trim().to_string()
            }
            _ => joined.trim().to_string(),
        };
        (!text.is_empty()).then_some(text)
    }
}

/// `a.b.0`: object keys and list indices. A key that itself contains a dot
/// is matched whole first, so a CSV header like `câu.hỏi` still resolves.
fn lookup<'a>(row: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(v) = row.get(path) {
        return Some(v);
    }
    let mut cur = row;
    for part in path.split('.') {
        cur = match cur {
            Value::Object(o) => o.get(part)?,
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        // A list or object needs an explicit index or key: guessing which
        // element was meant is how a dataset gets silently wrong labels.
        _ => None,
    }
}

/// Source label → SenClaw label, and the other labels also counted right
/// for a source label. A label missing from `map`, or mapped to `null`, is
/// dropped on import as `unmapped_label`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LabelMap {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub map: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub accept: BTreeMap<String, Vec<String>>,
}

/// A mapping names only fields of its kind and covers the required ones.
pub fn check_mapping(kind: Kind, mapping: &BTreeMap<String, FieldSource>) -> Result<(), String> {
    if let Some(f) = mapping.keys().find(|f| !kind.allows(f)) {
        let fields: Vec<&str> = kind.fields().iter().map(|(f, _)| *f).collect();
        return Err(format!("\"{f}\" is not a field of {} (fields: id, {})", kind.name(), fields.join(", ")));
    }
    let missing: Vec<&str> = kind.fields().iter().filter(|(f, req)| *req && !mapping.contains_key(*f)).map(|(f, _)| *f).collect();
    if !missing.is_empty() {
        return Err(format!("map a column to {} for {}", missing.join(", "), kind.name()));
    }
    for src in mapping.values() {
        if src.paths().iter().any(|p| p.trim().is_empty()) {
            return Err("a mapping names an empty column".into());
        }
    }
    Ok(())
}

/// One source row as a record, or the reason it was dropped. `index` is
/// the row's position in the source, used as the id when none is mapped.
pub fn normalize(
    row: &Value,
    index: u64,
    kind: Kind,
    mapping: &BTreeMap<String, FieldSource>,
    labels: Option<&LabelMap>,
) -> Result<Value, String> {
    let mut vals: BTreeMap<&str, String> = BTreeMap::new();
    for (field, required) in kind.fields() {
        match mapping.get(*field).and_then(|s| s.extract(row)) {
            Some(v) => {
                let cap = if *field == "context" { MAX_CONTEXT_CHARS } else { MAX_FIELD_CHARS };
                if v.chars().count() > cap {
                    return Err("too_long".into());
                }
                vals.insert(field, v);
            }
            None if *required => return Err(format!("missing_{field}")),
            None => {}
        }
    }
    let id = mapping.get("id").and_then(|s| s.extract(row)).unwrap_or_else(|| (index + 1).to_string());
    let mut rec = Map::new();
    rec.insert("id".into(), json!(id));
    match kind {
        Kind::Chat => {
            let mut messages = Vec::new();
            if let Some(s) = vals.get("system") {
                messages.push(json!({"role": "system", "content": s}));
            }
            messages.push(json!({"role": "user", "content": vals["user"]}));
            messages.push(json!({"role": "assistant", "content": vals["assistant"]}));
            rec.insert("messages".into(), Value::Array(messages));
        }
        _ => {
            for (field, _) in kind.fields() {
                if let Some(v) = vals.get(field) {
                    rec.insert((*field).into(), json!(v));
                }
            }
        }
    }
    if let (Some(lm), Some(source_label)) = (labels, vals.get("label")) {
        let Some(Some(label)) = lm.map.get(source_label.as_str()) else { return Err("unmapped_label".into()) };
        rec.insert("label".into(), json!(label));
        rec.insert("sourceLabel".into(), json!(source_label));
        if let Some(extra) = lm.accept.get(source_label.as_str()) {
            let mut accept = vec![label.clone()];
            accept.extend(extra.iter().filter(|l| *l != label).cloned());
            rec.insert("accept".into(), json!(accept));
        }
    }
    rec.insert("source".into(), json!({ "row": index }));
    Ok(Value::Object(rec))
}

/// The content of a record that makes two rows duplicates: everything but
/// its id and where it came from.
pub fn content_key(rec: &Value) -> String {
    let mut r = rec.clone();
    if let Some(o) = r.as_object_mut() {
        o.remove("id");
        o.remove("source");
    }
    r.to_string()
}

/// Column names commonly used for each field, best first.
fn candidates(field: &str) -> &'static [&'static str] {
    match field {
        "id" => &["id", "_id", "uid", "guid"],
        "text" => &["text", "instruction", "utterance", "sentence", "prompt", "query", "input", "content", "question", "cau", "noi_dung"],
        // A Hugging Face ClassLabel is an integer `label` with its name in
        // `label_text`; the name is the one worth keeping.
        "label" => &["label_text", "label", "intent", "category", "class", "labels", "nhan", "loai"],
        "question" => &["question", "query", "q", "cau_hoi"],
        "answer" => &["answer", "answers.text.0", "final_answer", "a", "output", "response", "dap_an", "tra_loi"],
        "context" => &["context", "passage", "document", "paragraph", "doan_van"],
        "problem" => &["problem", "question", "bai_toan", "de_bai"],
        "user" => &["instruction", "prompt", "question", "input", "user"],
        "assistant" => &["response", "output", "answer", "completion", "assistant"],
        "system" => &["system", "system_prompt"],
        _ => &[],
    }
}

/// A guess at the kind and mapping from column names, for the import
/// preview and for an import that named a kind but no mapping.
pub fn suggest(columns: &[String], kind: Option<Kind>) -> (Kind, BTreeMap<String, FieldSource>) {
    // "Nội dung" and "noi_dung" are the same column name for this purpose.
    let lower: Vec<String> = columns
        .iter()
        .map(|c| {
            crate::security::replication::fold(c.trim())
                .to_lowercase()
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() || ch == '.' { ch } else { '_' })
                .collect()
        })
        .collect();
    let has = |names: &[&str]| names.iter().any(|n| lower.iter().any(|c| c == n));
    let kind = kind.unwrap_or_else(|| {
        if has(&["context", "passage", "paragraph", "doan_van"]) && has(&["question", "query", "cau_hoi"]) {
            Kind::QaContext
        } else if has(&["response", "completion"]) || (has(&["instruction", "prompt"]) && has(&["output"])) {
            Kind::Chat
        } else if has(&["problem", "bai_toan", "de_bai"]) || (has(&["question"]) && has(&["answer", "final_answer"])) {
            Kind::ProblemAnswer
        } else {
            Kind::LabeledText
        }
    });
    let mut used: HashSet<usize> = HashSet::new();
    let mut mapping = BTreeMap::new();
    let mut fields: Vec<&str> = vec!["id"];
    fields.extend(kind.fields().iter().map(|(f, _)| *f));
    for field in fields {
        let pick = candidates(field).iter().find_map(|name| lower.iter().enumerate().find(|(i, c)| c == name && !used.contains(i)).map(|(i, _)| i));
        if let Some(i) = pick {
            used.insert(i);
            mapping.insert(field.to_string(), FieldSource::Path(columns[i].clone()));
        }
    }
    (kind, mapping)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, &str)]) -> BTreeMap<String, FieldSource> {
        pairs.iter().map(|(k, v)| (k.to_string(), FieldSource::Path(v.to_string()))).collect()
    }

    #[test]
    fn paths_reach_into_objects_and_lists_and_markers_cut() {
        let row = json!({"answers": {"text": ["Hà Nội", "HN"]}, "answer": "16 - 3 = 13\n#### 13", "n": 7, "câu.hỏi": "x"});
        assert_eq!(FieldSource::Path("answers.text.0".into()).extract(&row).as_deref(), Some("Hà Nội"));
        assert_eq!(FieldSource::Path("answers.text".into()).extract(&row), None, "a list needs an index");
        assert_eq!(FieldSource::Path("n".into()).extract(&row).as_deref(), Some("7"));
        assert_eq!(FieldSource::Path("câu.hỏi".into()).extract(&row).as_deref(), Some("x"));
        let after: FieldSource = serde_json::from_value(json!({"from": "answer", "after": "####"})).unwrap();
        assert_eq!(after.extract(&row).as_deref(), Some("13"));
        let missing: FieldSource = serde_json::from_value(json!({"from": "n", "after": "####"})).unwrap();
        assert_eq!(missing.extract(&row), None);
        let joined: FieldSource = serde_json::from_value(json!({"from": ["n", "nope", "answers.text.1"]})).unwrap();
        assert_eq!(joined.extract(&row).as_deref(), Some("7\n\nHN"));
        assert!(serde_json::from_value::<FieldSource>(json!({"from": "a", "afer": "x"})).is_err());
    }

    #[test]
    fn rows_become_records_of_their_kind() {
        let row = json!({"id": "q1", "question": "Thủ đô?", "answers": {"text": ["Hà Nội"]}, "context": "Hà Nội là thủ đô."});
        let rec = normalize(&row, 4, Kind::QaContext, &m(&[("id", "id"), ("question", "question"), ("answer", "answers.text.0"), ("context", "context")]), None).unwrap();
        assert_eq!(rec, json!({"id": "q1", "question": "Thủ đô?", "answer": "Hà Nội", "context": "Hà Nội là thủ đô.", "source": {"row": 4}}));

        let chat = normalize(&json!({"instruction": "hi", "response": "hello"}), 0, Kind::Chat, &m(&[("user", "instruction"), ("assistant", "response")]), None).unwrap();
        assert_eq!(chat["id"], "1");
        assert_eq!(chat["messages"], json!([{"role": "user", "content": "hi"}, {"role": "assistant", "content": "hello"}]));

        let err = normalize(&json!({"text": "  "}), 0, Kind::LabeledText, &m(&[("text", "text")]), None).unwrap_err();
        assert_eq!(err, "missing_text");
        let long = "x".repeat(MAX_FIELD_CHARS + 1);
        assert_eq!(normalize(&json!({"text": long}), 0, Kind::LabeledText, &m(&[("text", "text")]), None).unwrap_err(), "too_long");
    }

    #[test]
    fn a_label_map_renames_keeps_the_source_label_and_drops_the_unmapped() {
        let lm: LabelMap = serde_json::from_value(json!({
            "map": {"weather_query": "web-research", "general_joke": "none", "alarm_set": null},
            "accept": {"weather_query": ["agent-browser"]}
        }))
        .unwrap();
        let map = m(&[("text", "text"), ("label", "label")]);
        let rec = normalize(&json!({"text": "trời có mưa không", "label": "weather_query"}), 0, Kind::LabeledText, &map, Some(&lm)).unwrap();
        assert_eq!(rec["label"], "web-research");
        assert_eq!(rec["sourceLabel"], "weather_query");
        assert_eq!(rec["accept"], json!(["web-research", "agent-browser"]));
        let joke = normalize(&json!({"text": "kể chuyện cười", "label": "general_joke"}), 0, Kind::LabeledText, &map, Some(&lm)).unwrap();
        assert!(joke.get("accept").is_none());
        for label in ["alarm_set", "play_music"] {
            assert_eq!(normalize(&json!({"text": "x y", "label": label}), 0, Kind::LabeledText, &map, Some(&lm)).unwrap_err(), "unmapped_label");
        }
    }

    #[test]
    fn mappings_are_checked_against_the_kind() {
        assert!(check_mapping(Kind::LabeledText, &m(&[("text", "a"), ("id", "b")])).is_ok());
        assert!(check_mapping(Kind::LabeledText, &m(&[("label", "a")])).unwrap_err().contains("text"));
        assert!(check_mapping(Kind::ProblemAnswer, &m(&[("problem", "a"), ("context", "b")])).unwrap_err().contains("\"context\""));
    }

    #[test]
    fn suggestions_follow_common_column_names() {
        let cols = |c: &[&str]| c.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (k, map) = suggest(&cols(&["id", "label", "label_text", "text", "lang"]), None);
        assert_eq!(k, Kind::LabeledText);
        assert_eq!(map, m(&[("id", "id"), ("text", "text"), ("label", "label_text")]));
        let (k, map) = suggest(&cols(&["id", "context", "question", "answers.text.0", "answers.answer_start.0"]), None);
        assert_eq!(k, Kind::QaContext);
        assert_eq!(map["answer"], FieldSource::Path("answers.text.0".into()));
        let (k, map) = suggest(&cols(&["question", "answer"]), None);
        assert_eq!(k, Kind::ProblemAnswer);
        assert_eq!(map, m(&[("problem", "question"), ("answer", "answer")]));
        let (k, _) = suggest(&cols(&["instruction", "context", "response", "category"]), None);
        assert_eq!(k, Kind::Chat);
        let (_, map) = suggest(&cols(&["Nội dung", "Loại"]), Some(Kind::LabeledText));
        assert_eq!(map, m(&[("text", "Nội dung"), ("label", "Loại")]));
    }
}

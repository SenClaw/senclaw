//! Observation → decision request.
//!
//! One index per observed element; each operation (CLICK, TYPE_TEXT, SELECT)
//! gets its own target question listing only elements that support it, and a
//! single request asks every question at once (speculative fan-out): only the
//! head matching the chosen operation is ever used. This is a byte-for-byte
//! port of browser-use/jev-ultrafast's `action_space()` + `choose()` body — the
//! tests compare against fixtures produced by that Python — plus the "format
//! v3" compaction the local `laya-browser` checkpoint was fine-tuned on
//! (Laya-jev `browser_head.to_format_v3`).
//!
//! Everything is built as the order-preserving [`Json`]: the order of options
//! and keys is part of what the model reads.

use serde_json::Value;

use crate::decision::json::Json;
use crate::decision::types::{AskRequest, Backend};

use super::prompts::{BLOCKED_LABEL, DONE_LABEL, NEXT_ACTION, OPERATION_LABELS, TARGET};

/// Which wire format the decision backend was trained / measured on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    /// Hosted Jev: full element table in state, rich target options.
    JevFull,
    /// Local laya-browser: 1,200 characters of page text, one line per option.
    LayaV3,
}

/// Controls the laya-browser checkpoint was trained with. Anything else
/// (Enter, back, dialog answers) goes to the LLM tier on that backend.
const LAYA_CONTROLS: [&str; 3] = ["SCROLL_UP", "SCROLL_DOWN", "WAIT"];
const PAGE_TEXT_V3: usize = 1200;
const LABEL_CHARS_V3: usize = 50;
const VALUE_CHARS_V3: usize = 30;

/// One step the agent already executed, as the model sees it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct HistoryItem {
    pub action: String,
    pub kind: String,
    pub text: Option<String>,
    pub page_changed: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Target {
    /// `"7"`, or `"4:2"` for the second option of dropdown 4.
    pub index: String,
    pub action: Value,
}

/// The indexed action space of one observation.
#[derive(Debug, Clone, Default)]
pub struct Space {
    pub elements: Vec<Json>,
    /// Operation → its candidates, in order of first appearance.
    pub targets: Vec<(String, Vec<Target>)>,
    /// `SCROLL_DOWN`, `WAIT`, `KEY_ENTER`, … → the observed control.
    pub controls: Vec<(String, Value)>,
}

impl Space {
    pub fn target(&self, operation: &str, index: &str) -> Option<&Target> {
        self.targets.iter().find(|(op, _)| op == operation)?.1.iter().find(|t| t.index == index)
    }

    pub fn control(&self, operation: &str) -> Option<&Value> {
        self.controls.iter().find(|(op, _)| op == operation).map(|(_, a)| a)
    }

    pub fn operations_with_targets(&self) -> impl Iterator<Item = &str> {
        self.targets.iter().map(|(op, _)| op.as_str())
    }
}

pub struct Encoded {
    pub request: AskRequest,
    pub space: Space,
    /// Offered operations, in the order the operation question lists them.
    pub operations: Vec<String>,
}

fn s(text: impl Into<String>) -> Json {
    Json::String(text.into())
}

fn obj(entries: Vec<(&str, Json)>) -> Json {
    Json::Object(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// serde_json → ordered Json (object keys as serde_json iterates them; only
/// scalars reach here in practice).
pub fn to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Number(n) => Json::Number(n.clone()),
        Value::String(t) => Json::String(t.clone()),
        Value::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        Value::Object(map) => Json::Object(map.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
    }
}

fn set(entries: &mut Vec<(String, Json)>, key: &str, value: Json) {
    match entries.iter_mut().find(|(k, _)| k == key) {
        Some((_, v)) => *v = value,
        None => entries.push((key.to_string(), value)),
    }
}

fn operation_of(kind: &str) -> Option<&'static str> {
    match kind {
        "click" => Some("CLICK"),
        "fill" => Some("TYPE_TEXT"),
        "select" => Some("SELECT"),
        _ => None,
    }
}

/// `action_space()` from jev-ultrafast: one index per node, per-operation targets.
pub fn action_space(actions: &[Value]) -> Space {
    let mut space = Space::default();
    let mut indices: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for action in actions {
        let kind = action.get("kind").and_then(Value::as_str).unwrap_or_default();
        let Some(operation) = operation_of(kind) else {
            let key = action.get("id").and_then(Value::as_str).unwrap_or_default().to_uppercase();
            match space.controls.iter_mut().find(|(k, _)| *k == key) {
                Some((_, a)) => *a = action.clone(),
                None => space.controls.push((key, action.clone())),
            }
            continue;
        };
        let node = action.get("node").map(|n| n.to_string()).unwrap_or_default();
        let position = match indices.get(&node) {
            Some(p) => *p,
            None => {
                let index = (space.elements.len() + 1).to_string();
                let mut entries: Vec<(String, Json)> = Vec::new();
                for k in ["role", "value", "checked", "selected", "expanded"] {
                    if let Some(v) = action.get(k) {
                        entries.push((k.to_string(), to_json(v)));
                    }
                }
                let label = action.get("label").and_then(Value::as_str).unwrap_or_default();
                let label = label.split(" → ").next().unwrap_or_default();
                entries.push(("index".into(), s(index.clone())));
                entries.push(("label".into(), s(label)));
                entries.push(("operations".into(), Json::Array(Vec::new())));
                if kind == "select" {
                    let current = action.get("current_value").cloned().unwrap_or(Value::String(String::new()));
                    set(&mut entries, "value", to_json(&current));
                    entries.push(("options".into(), Json::Array(Vec::new())));
                }
                space.elements.push(Json::Object(entries));
                let p = space.elements.len() - 1;
                indices.insert(node, p);
                p
            }
        };
        let index = (position + 1).to_string();
        let Json::Object(entries) = &mut space.elements[position] else { unreachable!() };
        if let Some((_, Json::Array(ops))) = entries.iter_mut().find(|(k, _)| k == "operations") {
            if !ops.iter().any(|o| o.as_str() == Some(operation)) {
                ops.push(s(operation));
            }
        }
        let mut target = index.clone();
        if kind == "select" {
            if let Some((_, Json::Array(options))) = entries.iter_mut().find(|(k, _)| k == "options") {
                target = format!("{index}:{}", options.len() + 1);
                options.push(obj(vec![
                    ("index", s(target.clone())),
                    ("label", to_json(action.get("label").unwrap_or(&Value::Null))),
                    ("value", to_json(action.get("value").unwrap_or(&Value::Null))),
                ]));
            }
        }
        let group = match space.targets.iter_mut().position(|(op, _)| op == operation) {
            Some(i) => &mut space.targets[i].1,
            None => {
                space.targets.push((operation.to_string(), Vec::new()));
                &mut space.targets.last_mut().expect("just pushed").1
            }
        };
        match group.iter_mut().find(|t| t.index == target) {
            Some(t) => t.action = action.clone(),
            None => group.push(Target { index: target, action: action.clone() }),
        }
    }
    space
}

fn target_option(t: &Target) -> Json {
    let a = &t.action;
    let label = a.get("label").and_then(Value::as_str).unwrap_or_default();
    let current = a
        .get("current_value")
        .or_else(|| a.get("value"))
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let mut entries = vec![
        ("element".to_string(), s(format!("[{}] {label}", t.index))),
        ("current_value".to_string(), to_json(&current)),
    ];
    for k in ["role", "checked", "selected", "expanded"] {
        if let Some(v) = a.get(k) {
            entries.push((k.to_string(), to_json(v)));
        }
    }
    Json::Object(entries)
}

/// Build the one request that asks every head.
pub fn encode(
    observation: &Value,
    goal: &str,
    history: &[HistoryItem],
    profile: Profile,
    model: Option<String>,
    backend: Option<Backend>,
) -> Encoded {
    let actions = observation.get("actions").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut space = action_space(&actions);
    if profile == Profile::LayaV3 {
        space.controls.retain(|(op, _)| LAYA_CONTROLS.contains(&op.as_str()));
    }

    let mut operations: Vec<(String, Json)> = Vec::new();
    for (op, _) in &space.targets {
        let label = OPERATION_LABELS.iter().find(|(k, _)| k == op).map(|(_, l)| *l).unwrap_or_default();
        operations.push((op.clone(), s(label)));
    }
    for (op, action) in &space.controls {
        operations.push((op.clone(), to_json(action.get("label").unwrap_or(&Value::Null))));
    }
    operations.push(("DONE".into(), s(DONE_LABEL)));
    operations.push(("BLOCKED".into(), s(BLOCKED_LABEL)));
    let offered: Vec<String> = operations.iter().map(|(k, _)| k.clone()).collect();

    let mut questions: Vec<(String, Json)> = vec![(
        "operation".into(),
        obj(vec![
            ("type", s("choice")),
            ("criteria", Json::Object(operations)),
            ("instructions", obj(vec![("goal", s(goal)), ("rules", s(NEXT_ACTION))])),
        ]),
    )];
    for (op, candidates) in &space.targets {
        let criteria: Vec<(String, Json)> = candidates
            .iter()
            .map(|t| {
                let option = target_option(t);
                let option = if profile == Profile::LayaV3 { compact_v3(&option) } else { option };
                (t.index.clone(), option)
            })
            .collect();
        questions.push((
            format!("{}_target", op.to_lowercase()),
            obj(vec![
                ("type", s("choice")),
                ("criteria", Json::Object(criteria)),
                (
                    "instructions",
                    obj(vec![
                        ("goal", s(goal)),
                        ("operation", s(op.clone())),
                        ("rules", Json::Array(vec![s(NEXT_ACTION), s(TARGET)])),
                    ]),
                ),
            ]),
        ));
    }

    let text = observation.get("text").and_then(Value::as_str).unwrap_or_default();
    let text: String = match profile {
        Profile::JevFull => text.to_string(),
        Profile::LayaV3 => text.chars().take(PAGE_TEXT_V3).collect(),
    };
    let page = obj(vec![
        ("url", to_json(observation.get("url").unwrap_or(&Value::String(String::new())))),
        ("title", to_json(observation.get("title").unwrap_or(&Value::String(String::new())))),
        ("text", s(text)),
    ]);
    let recent: Vec<Json> = history
        .iter()
        .rev()
        .take(10)
        .rev()
        .map(|h| {
            obj(vec![
                ("action", s(h.action.clone())),
                ("kind", s(h.kind.clone())),
                ("text", h.text.clone().map(Json::String).unwrap_or(Json::Null)),
                ("page_changed", h.page_changed.map(Json::Bool).unwrap_or(Json::Null)),
            ])
        })
        .collect();
    let state = match profile {
        Profile::JevFull => obj(vec![
            ("page", page),
            ("elements", Json::Array(space.elements.clone())),
            ("recent_actions", Json::Array(recent)),
        ]),
        Profile::LayaV3 => obj(vec![("page", page), ("recent_actions", Json::Array(recent))]),
    };

    Encoded {
        request: AskRequest { backend, model, state, questions: Json::Object(questions) },
        space,
        operations: offered,
    }
}

/// One line per option, as laya-browser was trained: `[3] Search (searchbox) = 'laya' checked=false`.
fn compact_v3(option: &Json) -> Json {
    let Some(element) = option.get("element") else { return option.clone() };
    let mut line: String = json_str(element).chars().take(LABEL_CHARS_V3).collect();
    if let Some(role) = option.get("role").filter(|r| truthy(r)) {
        line.push_str(&format!(" ({})", json_str(role)));
    }
    if let Some(value) = option.get("current_value").filter(|v| truthy(v)) {
        let clipped: String = json_str(value).chars().take(VALUE_CHARS_V3).collect();
        line.push_str(&format!(" = {}", py_repr(&clipped)));
    }
    for flag in ["checked", "selected", "expanded"] {
        if let Some(v) = option.get(flag) {
            line.push_str(&format!(" {flag}={}", py_str(v)));
        }
    }
    Json::String(line)
}

fn truthy(v: &Json) -> bool {
    match v {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::String(t) => !t.is_empty(),
        Json::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Json::Array(a) => !a.is_empty(),
        Json::Object(o) => !o.is_empty(),
    }
}

/// Python's `str()` of a JSON value.
fn json_str(v: &Json) -> String {
    match v {
        Json::String(t) => t.clone(),
        other => py_str(other),
    }
}

fn py_str(v: &Json) -> String {
    match v {
        Json::Null => "None".into(),
        Json::Bool(true) => "True".into(),
        Json::Bool(false) => "False".into(),
        Json::Number(n) => n.to_string(),
        Json::String(t) => t.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Python's `repr()` of a str.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        serde_json::from_str(if name == "jev" { JEV } else { LAYA }).unwrap()
    }

    /// A node of a fixture, read with the order-preserving parser.
    fn ordered(raw: &str, pointer: &str) -> Json {
        let whole: Json = serde_json::from_str(raw).unwrap();
        let mut node = &whole;
        for part in pointer.trim_start_matches('/').split('/') {
            node = node.get(part).unwrap_or_else(|| panic!("{pointer} missing in fixture"));
        }
        node.clone()
    }

    const JEV: &str = include_str!("testdata/jev_choose.json");
    const LAYA: &str = include_str!("testdata/laya_v3.json");

    fn inputs() -> (Value, String, Vec<HistoryItem>) {
        let f = fixture("jev");
        let history = f["history"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| HistoryItem {
                action: h["action"].as_str().unwrap().into(),
                kind: h["kind"].as_str().unwrap().into(),
                text: h["text"].as_str().map(str::to_string),
                page_changed: h["page_changed"].as_bool(),
            })
            .collect();
        (f["page"].clone(), f["goal"].as_str().unwrap().to_string(), history)
    }

    #[test]
    fn jev_request_matches_upstream_choose() {
        let (page, goal, history) = inputs();
        let encoded = encode(&page, &goal, &history, Profile::JevFull, Some("jev-1.13.0".into()), None);
        let f = fixture("jev");
        assert_eq!(encoded.request.state, ordered(JEV, "/body/state"));
        assert_eq!(encoded.request.questions, ordered(JEV, "/body/questions"));
        assert_eq!(encoded.request.model.as_deref(), f["body"]["model"].as_str());
        assert_eq!(encoded.operations, vec!["CLICK", "TYPE_TEXT", "SELECT", "SCROLL_DOWN", "WAIT", "DONE", "BLOCKED"]);
        // The dropdown's options are addressed as element:option.
        let pick = encoded.space.target("SELECT", "4:2").unwrap();
        assert_eq!(pick.action["id"], "e7");
        assert_eq!(encoded.space.target("TYPE_TEXT", "2").unwrap().action["id"], "e2");
        assert_eq!(encoded.space.target("CLICK", "2").unwrap().action["id"], "e3");
    }

    #[test]
    fn laya_v3_matches_upstream_format() {
        let (page, goal, history) = inputs();
        let encoded = encode(&page, &goal, &history, Profile::LayaV3, Some("laya-browser".into()), Some(Backend::Local));
        assert_eq!(encoded.request.state, ordered(LAYA, "/state"));
        assert_eq!(encoded.request.questions, ordered(LAYA, "/questions"));
    }

    #[test]
    fn laya_drops_controls_it_was_not_trained_on() {
        let page = serde_json::json!({
            "url": "https://a.test/", "title": "A", "text": "x",
            "actions": [
                {"id": "e1", "node": 1, "kind": "click", "role": "button", "label": "Go", "value": ""},
                {"id": "key_enter", "kind": "key", "key": "Enter", "label": "Press Enter in Search"},
                {"id": "go_back", "kind": "back", "label": "Go back to the previous page"},
                {"id": "wait", "kind": "wait", "label": "Wait for the page to update"}
            ]
        });
        let laya = encode(&page, "g", &[], Profile::LayaV3, None, None);
        assert_eq!(laya.operations, vec!["CLICK", "WAIT", "DONE", "BLOCKED"]);
        let jev = encode(&page, "g", &[], Profile::JevFull, None, None);
        assert_eq!(jev.operations, vec!["CLICK", "KEY_ENTER", "GO_BACK", "WAIT", "DONE", "BLOCKED"]);
    }

    #[test]
    fn python_repr_rules() {
        assert_eq!(py_repr("San Francisco"), "'San Francisco'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("Zürich"), "'Zürich'");
        assert_eq!(py_repr("a\nb"), "'a\\nb'");
    }
}

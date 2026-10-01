//! Observation → decision request.
//!
//! One index per observed element; each operation (CLICK, TYPE_TEXT, SELECT)
//! gets its own target question listing only elements that support it, and a
//! single request asks every question at once (speculative fan-out): only the
//! head matching the chosen operation is ever used. This is a byte-for-byte
//! port of browser-use/jev-ultrafast's `action_space()` + `choose()` body — the
//! tests compare against fixtures produced by that Python — plus the
//! compaction the local `laya-browser` checkpoints were fine-tuned on: "format
//! v3" (Laya-jev `browser_head.to_format_v3`) up to v10s, "format v5"
//! (`build_request` in the model card's `laya_browser.py`) from v19s on.
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
    /// Local laya-browser up to v10s: 1,200 characters of page text, one line per option.
    LayaV3,
    /// Local laya-browser from v19s on: v3 without the `[index] ` before each
    /// option (laya renders the key itself), a dropdown option as just
    /// "Field → Option", and the form's fields with their current values
    /// first in the state.
    LayaV5,
}

impl Profile {
    /// The format a local checkpoint was fine-tuned on, from `laya_fmt` in its
    /// `rl_agent_config.json`. A checkpoint that does not say is read as v3,
    /// the format of the checkpoints from before the field existed.
    pub fn for_laya_format(laya_fmt: Option<&str>) -> Profile {
        match laya_fmt {
            Some("v5") => Profile::LayaV5,
            _ => Profile::LayaV3,
        }
    }
}

/// Controls the v3 checkpoint was trained with. Anything else (Enter, back,
/// dialog answers) goes to the LLM tier on that backend.
const LAYA_CONTROLS: [&str; 3] = ["SCROLL_UP", "SCROLL_DOWN", "WAIT"];
/// Controls the v5 checkpoint was trained with — Enter among them, under the
/// name and label its harness gave it.
const LAYA_V5_CONTROLS: [&str; 4] = ["SCROLL_DOWN", "SCROLL_UP", "KEY_ENTER", "WAIT"];
const PRESS_ENTER: &str = "PRESS_ENTER";
const PRESS_ENTER_LABEL: &str = "Press Enter in the focused text field (submit it)";
const PAGE_TEXT_V3: usize = 1200;
const LABEL_CHARS_V3: usize = 50;
const VALUE_CHARS_V3: usize = 30;
/// `fields_summary` of format v5: label and value lengths, and at most this many fields.
const FIELD_LABEL_CHARS: usize = 40;
const MAX_FIELDS: usize = 14;

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
    /// `(name in the request, name in the loop)` for an operation the model
    /// knows by another name (`PRESS_ENTER` for `KEY_ENTER` in format v5).
    pub aliases: Vec<(String, String)>,
}

impl Encoded {
    /// An answer of the operation question with the request's names turned
    /// back into the loop's, so the risk tiers see the operation they know.
    pub fn loop_names(&self, answer: &Value) -> Value {
        if self.aliases.is_empty() {
            return answer.clone();
        }
        let rename = |name: &str| {
            self.aliases.iter().find(|(wire, _)| wire == name).map(|(_, own)| own.clone()).unwrap_or_else(|| name.to_string())
        };
        let mut out = answer.clone();
        if let Some(choice) = answer.get("choice").and_then(Value::as_str) {
            out["choice"] = Value::String(rename(choice));
        }
        if let Some(probabilities) = answer.get("probabilities").and_then(Value::as_object) {
            out["probabilities"] = Value::Object(probabilities.iter().map(|(k, v)| (rename(k), v.clone())).collect());
        }
        out
    }
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
    match profile {
        Profile::JevFull => {}
        Profile::LayaV3 => space.controls.retain(|(op, _)| LAYA_CONTROLS.contains(&op.as_str())),
        Profile::LayaV5 => space.controls.retain(|(op, control)| {
            LAYA_V5_CONTROLS.contains(&op.as_str()) && (op != "KEY_ENTER" || focused_field_has_text(&actions, control))
        }),
    }

    let mut operations: Vec<(String, Json)> = Vec::new();
    let mut offered: Vec<String> = Vec::new();
    let mut aliases: Vec<(String, String)> = Vec::new();
    for (op, _) in &space.targets {
        let label = OPERATION_LABELS.iter().find(|(k, _)| k == op).map(|(_, l)| *l).unwrap_or_default();
        operations.push((op.clone(), s(label)));
        offered.push(op.clone());
    }
    for (op, action) in &space.controls {
        if profile == Profile::LayaV5 && op == "KEY_ENTER" {
            operations.push((PRESS_ENTER.into(), s(PRESS_ENTER_LABEL)));
            aliases.push((PRESS_ENTER.into(), op.clone()));
        } else {
            operations.push((op.clone(), to_json(action.get("label").unwrap_or(&Value::Null))));
        }
        offered.push(op.clone());
    }
    operations.push(("DONE".into(), s(DONE_LABEL)));
    operations.push(("BLOCKED".into(), s(BLOCKED_LABEL)));
    offered.extend(["DONE".to_string(), "BLOCKED".to_string()]);

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
                let option = match profile {
                    Profile::JevFull => option,
                    Profile::LayaV3 => compact_v3(&option),
                    Profile::LayaV5 => compact_v5(&option),
                };
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
        Profile::LayaV3 | Profile::LayaV5 => text.chars().take(PAGE_TEXT_V3).collect(),
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
            // The v5 harness logged Enter under its control's own label.
            let action = if profile == Profile::LayaV5 && h.kind == "key" && h.action.starts_with("Press Enter") {
                PRESS_ENTER_LABEL.to_string()
            } else {
                h.action.clone()
            };
            obj(vec![
                ("action", s(action)),
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
        Profile::LayaV5 => obj(vec![
            ("fields", s(fields_summary(&space.elements))),
            ("page", page),
            ("recent_actions", Json::Array(recent)),
        ]),
    };

    Encoded {
        request: AskRequest { backend, model, state, questions: Json::Object(questions) },
        space,
        operations: offered,
        aliases,
    }
}

/// The v5 harness offered Enter only in a focused text field holding text.
/// The observation names the focused field in the control's label ("Press
/// Enter in Search", the field's own name or role), and the field's `fill`
/// action carries its value.
fn focused_field_has_text(actions: &[Value], enter: &Value) -> bool {
    let Some(name) = enter.get("label").and_then(Value::as_str).and_then(|l| l.strip_prefix("Press Enter in ")) else {
        return false;
    };
    actions.iter().any(|a| {
        a.get("kind").and_then(Value::as_str) == Some("fill")
            && a.get("label").and_then(Value::as_str) == Some(name)
            && a.get("value").is_some_and(|v| !py_strip(&value_text(&to_json(v))).is_empty())
    })
}

/// Every form field with its current value, then the checked state of
/// checkboxes, radios and switches, in page order — at most 14
/// (`fields_summary` of format v5).
fn fields_summary(elements: &[Json]) -> String {
    let mut out: Vec<String> = Vec::new();
    for e in elements {
        let operations: Vec<&str> = match e.get("operations") {
            Some(Json::Array(ops)) => ops.iter().filter_map(Json::as_str).collect(),
            _ => Vec::new(),
        };
        let role = e.get("role").and_then(Json::as_str);
        let label: String = e.get("label").map(json_str).unwrap_or_default().chars().take(FIELD_LABEL_CHARS).collect();
        if operations.contains(&"TYPE_TEXT") || operations.contains(&"SELECT") || role == Some("combobox") {
            let value = py_strip(&e.get("value").map(value_text).unwrap_or_default());
            if value.is_empty() {
                out.push(format!("{label} = (empty)"));
            } else {
                let clipped: String = value.chars().take(VALUE_CHARS_V3).collect();
                out.push(format!("{label} = {}", py_repr(&clipped)));
            }
        } else if matches!(role, Some("checkbox" | "radio" | "switch")) {
            if let Some(checked) = e.get("checked") {
                out.push(format!("{label}: checked={}", py_str(checked)));
            }
        }
        if out.len() >= MAX_FIELDS {
            break;
        }
    }
    out.join("; ")
}

/// Python's `str(v or "")`.
fn value_text(v: &Json) -> String {
    if truthy(v) {
        json_str(v)
    } else {
        String::new()
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

/// One line per option in format v5: v3 without the `[3] ` before the label
/// (laya renders the key itself), and a dropdown option as only
/// "Field → Option" — its role and current value repeated on every option
/// ate the head's budget.
fn compact_v5(option: &Json) -> Json {
    let Some(element) = option.get("element") else { return option.clone() };
    let element = strip_index(&json_str(element));
    let mut line = cut(&element, LABEL_CHARS_V3);
    if element.contains(" → ") {
        return Json::String(line);
    }
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

/// `re.sub(r"^\[[^\]]*\]\s*", "", label)`.
fn strip_index(label: &str) -> String {
    match label.strip_prefix('[').and_then(|rest| rest.find(']').map(|end| &rest[end + 1..])) {
        Some(rest) => rest.trim_start_matches(py_isspace).to_string(),
        None => label.to_string(),
    }
}

/// `_cut`: at most `n` characters, except that a dropdown option ("Field →
/// Option") keeps its option name and gives up the field's.
fn cut(label: &str, n: usize) -> String {
    if label.chars().count() <= n {
        return label.to_string();
    }
    if let Some((field, option)) = label.rsplit_once(" → ") {
        let option: String = py_split(option).join(" ").chars().take(40).collect();
        let keep = n.saturating_sub(option.chars().count() + 3).max(12);
        let field: String = py_split(field).join(" ").chars().take(keep).collect();
        return format!("{field} → {option}");
    }
    label.chars().take(n).collect()
}

/// Python's `str.isspace()` for one character: Rust's White_Space plus the
/// four information separators (U+001C–U+001F) Python also counts.
fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python's `str.strip()`.
fn py_strip(text: &str) -> String {
    text.trim_matches(py_isspace).to_string()
}

/// Python's `str.split()`: runs of whitespace separate, none are kept.
fn py_split(text: &str) -> Vec<&str> {
    text.split(py_isspace).filter(|w| !w.is_empty()).collect()
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
            c if !py_printable(c) => match c as u32 {
                n @ 0..=0xff => out.push_str(&format!("\\x{n:02x}")),
                n @ 0x100..=0xffff => out.push_str(&format!("\\u{n:04x}")),
                n => out.push_str(&format!("\\U{n:08x}")),
            },
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's `str.isprintable()` for one character, which `repr` escapes
/// when false: control and format characters, private use, the
/// noncharacters, and every separator but the ASCII space. A no-break space
/// (U+00A0) is common in prices and counts on the web. Code points that are
/// unassigned in Python's Unicode tables are not listed and pass through.
fn py_printable(c: char) -> bool {
    let n = c as u32;
    !(n < 0x20
        || (0x7f..=0xa0).contains(&n)
        || n == 0xad
        || (0x600..=0x605).contains(&n)
        || n == 0x61c
        || n == 0x6dd
        || n == 0x70f
        || (0x890..=0x891).contains(&n)
        || n == 0x8e2
        || n == 0x1680
        || n == 0x180e
        || (0x2000..=0x200f).contains(&n)
        || (0x2028..=0x202f).contains(&n)
        || (0x205f..=0x206f).contains(&n)
        || n == 0x3000
        || (0xe000..=0xf8ff).contains(&n)
        || (0xfdd0..=0xfdef).contains(&n)
        || n == 0xfeff
        || (0xfff9..=0xfffb).contains(&n)
        || n & 0xfffe == 0xfffe
        || n == 0x110bd
        || n == 0x110cd
        || (0x13430..=0x1343f).contains(&n)
        || (0x1bca0..=0x1bca3).contains(&n)
        || (0x1d173..=0x1d17a).contains(&n)
        || n == 0xe0001
        || (0xe0020..=0xe007f).contains(&n)
        || n >= 0xf0000)
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
        // What Python escapes beyond ASCII: separators and format characters.
        assert_eq!(py_repr("1\u{a0}000 ₫"), "'1\\xa0000 ₫'");
        assert_eq!(py_repr("a\u{200b}b\u{2028}c\u{feff}"), "'a\\u200bb\\u2028c\\ufeff'");
        assert_eq!(py_repr("\u{85}\u{ad}"), "'\\x85\\xad'");
        assert_eq!(py_repr("Thích 🍜 ̀"), "'Thích 🍜 ̀'");
        assert_eq!(py_repr("\u{f0000}"), "'\\U000f0000'");
    }

    const LAYA_V5: &str = include_str!("testdata/laya_v5.json");

    /// A node of the v5 fixture, read with the order-preserving parser.
    fn v5_node(path: &[&str]) -> Json {
        let mut node: Json = serde_json::from_str(LAYA_V5).unwrap();
        for part in path {
            node = match (part.parse::<usize>(), node) {
                (Ok(i), Json::Array(items)) => items[i].clone(),
                (_, other) => other.get(part).unwrap_or_else(|| panic!("{path:?} missing in the fixture")).clone(),
            };
        }
        node
    }

    fn v5_cases() -> Vec<Value> {
        let whole: Value = serde_json::from_str(LAYA_V5).unwrap();
        whole["cases"].as_array().unwrap().clone()
    }

    fn encode_v5(case: &Value) -> Encoded {
        let history: Vec<HistoryItem> = serde_json::from_value(case["history"].clone()).unwrap();
        encode(&case["page"], case["goal"].as_str().unwrap(), &history, Profile::LayaV5, Some("laya-browser".into()), Some(Backend::Local))
    }

    #[test]
    fn laya_v5_matches_the_requests_the_checkpoint_was_trained_on() {
        for (i, case) in v5_cases().iter().enumerate() {
            let name = case["name"].as_str().unwrap();
            let encoded = encode_v5(case);
            let i = i.to_string();
            assert_eq!(encoded.request.state, v5_node(&["cases", &i, "state"]), "{name}: state");
            assert_eq!(encoded.request.questions, v5_node(&["cases", &i, "questions"]), "{name}: questions");
        }
    }

    #[test]
    fn laya_v5_offers_enter_by_its_trained_name_only_in_a_field_holding_text() {
        let cases = v5_cases();
        let by_name = |n: &str| cases.iter().find(|c| c["name"] == n).unwrap();
        let search = encode_v5(by_name("search"));
        assert_eq!(search.operations, vec!["TYPE_TEXT", "CLICK", "SCROLL_DOWN", "KEY_ENTER", "WAIT", "DONE", "BLOCKED"]);
        assert_eq!(search.aliases, vec![("PRESS_ENTER".to_string(), "KEY_ENTER".to_string())]);
        // The focused "Where to?" is empty, and "go back" was never trained.
        let feed = encode_v5(by_name("feed"));
        assert!(!feed.operations.iter().any(|o| o == "KEY_ENTER" || o == "GO_BACK"), "{:?}", feed.operations);
        assert!(encode_v5(by_name("form")).aliases.is_empty());

        // The answer comes back under the request's name; the loop gets its own.
        let answers = serde_json::json!({
            "operation": {"type": "choice", "choice": "PRESS_ENTER", "confidence": 0.9, "probabilities": {
                "TYPE_TEXT": 0.02, "CLICK": 0.03, "SCROLL_DOWN": 0.01, "PRESS_ENTER": 0.9, "WAIT": 0.02, "DONE": 0.01, "BLOCKED": 0.01}}
        });
        let bands = super::super::decide::Bands { act: 0.6, fallback: 0.2 };
        let step = super::super::decide::resolve(&answers, &search, bands).unwrap();
        assert_eq!((step.operation.as_str(), step.action_id.as_str()), ("KEY_ENTER", "key_enter"));
    }

    #[test]
    fn a_checkpoint_says_which_format_it_reads() {
        assert_eq!(Profile::for_laya_format(Some("v5")), Profile::LayaV5);
        assert_eq!(Profile::for_laya_format(Some("v3")), Profile::LayaV3);
        assert_eq!(Profile::for_laya_format(None), Profile::LayaV3);
    }
}

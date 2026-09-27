//! Install-time parameters — the questions a kit asks before it is installed.
//!
//! A kit that hardcodes the folder it runs in, the API key it talks to, or how
//! many times a workflow repeats is a kit only its author can install. Declaring
//! those as `params` turns the manifest into a template the client renders as a
//! form, and the daemon substitutes the answers before anything reaches disk.
//!
//! ```json
//! "params": [
//!   { "key": "workdir", "type": "folder", "label": "Working folder", "required": true },
//!   { "key": "apiKey",  "type": "string", "label": "API key", "secret": true },
//!   { "key": "runs",    "type": "number", "label": "Runs", "default": 3, "min": 1, "max": 10 },
//!   { "key": "notify",  "type": "boolean", "label": "Notify when done", "default": true },
//!   { "key": "tier",    "type": "select", "label": "Tier",
//!     "options": [{ "value": "fast", "label": "Fast" }, { "value": "deep", "label": "Deep" }] }
//! ]
//! ```
//!
//! Placeholders are namespaced — `{{param.workdir}}`, never bare `{{workdir}}`.
//! Skill and workflow bodies are Markdown that legitimately contains other
//! `{{…}}` syntax (a workflow's own template vars, a handlebars snippet in a
//! code fence); an un-namespaced substitution would corrupt those silently. For
//! the same reason an unknown `{{param.x}}` is **left verbatim and warned
//! about**, never blanked: a blank is indistinguishable from an intentionally
//! empty value once it is on disk.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What a parameter accepts, which is also which control the clients render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum KitParamType {
    #[default]
    String,
    Number,
    Boolean,
    Select,
    /// A directory on this machine. Same wire type as `String`; separate so the
    /// clients can offer a native folder picker instead of a bare text field.
    Folder,
}

impl KitParamType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Select => "select",
            Self::Folder => "folder",
        }
    }
}

/// One option of a `select` parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitParamOption {
    pub value: String,
    /// Falls back to `value` when the author gave no label.
    #[serde(default)]
    pub label: String,
}

/// One declared parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KitParam {
    /// Placeholder name: `{{param.<key>}}`.
    pub key: String,
    /// Shown next to the control. Falls back to `key`.
    #[serde(default)]
    pub label: String,
    #[serde(default, rename = "type")]
    pub kind: KitParamType,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub placeholder: String,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub required: bool,
    /// Render masked and keep out of the receipt. Advisory only — see
    /// [`KitParam::is_secret`].
    #[serde(default)]
    pub secret: bool,
    /// `select` only.
    #[serde(default)]
    pub options: Vec<KitParamOption>,
    /// `number` only.
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
}

impl KitParam {
    /// A key usable as `{{param.<key>}}`: letters, digits, `_`, `-`, `.` would
    /// be ambiguous inside the namespaced placeholder, so it is excluded.
    pub fn key_is_valid(key: &str) -> bool {
        !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    /// Secrets are not written to the receipt. The flag is how an author says
    /// "this is a credential"; a key-shaped name is not enough to infer it, and
    /// guessing wrong in the other direction would drop values the user wants
    /// to see again.
    pub fn is_secret(&self) -> bool {
        self.secret
    }

    pub fn display_label(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.key
        } else {
            &self.label
        }
    }
}

/// Why a set of answers cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KitParamError(pub String);

impl std::fmt::Display for KitParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A validated answer, already rendered as the text that goes into the kit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KitParamValue {
    pub text: String,
    pub secret: bool,
}

/// Answers keyed by param key, resolved against the declarations.
pub type KitParamValues = BTreeMap<String, KitParamValue>;

/// Render a JSON answer as the text substituted into the manifest.
///
/// Numbers lose a pointless `.0` — `{{param.runs}}` in a cron line or a prompt
/// must read `3`, not `3.0`, and JSON has no integer/float distinction to lean
/// on.
fn render(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Validate one answer against its declaration, returning the text to splice in.
fn resolve_one(
    param: &KitParam,
    supplied: Option<&serde_json::Value>,
) -> Result<Option<String>, KitParamError> {
    let label = param.display_label().to_string();

    // An explicit JSON null means "not answered", the same as an absent key —
    // clients serialise a cleared field either way.
    let supplied = supplied.filter(|v| !v.is_null());
    let effective = supplied.or(param.default.as_ref().filter(|v| !v.is_null()));

    let Some(value) = effective else {
        if param.required {
            return Err(KitParamError(format!("\"{label}\" is required")));
        }
        // Declared but unanswered substitutes as empty, and does **not** fall
        // through to the leave-verbatim rule. That rule exists for a
        // placeholder no param declares — an author's typo, which blanking
        // would hide. Here the author declared the field and made it optional,
        // so they accepted that it may be absent; leaving `{{param.x}}` in a
        // prompt would put a template artifact in front of the model. An
        // author who wants a fallback declares `default`, which is what it is
        // for. It also keeps a never-touched field and a cleared one identical
        // — the form sends "" for one and nothing for the other.
        return Ok(Some(String::new()));
    };

    let text = render(value);

    match param.kind {
        KitParamType::Number => {
            // Accept a JSON number or a numeric string: an HTML number input
            // hands back a string, and rejecting that would fail every install
            // from the web UI.
            let parsed = match value {
                serde_json::Value::Number(n) => n.as_f64(),
                serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            };
            let Some(n) = parsed else {
                return Err(KitParamError(format!("\"{label}\" must be a number")));
            };
            if let Some(min) = param.min {
                if n < min {
                    return Err(KitParamError(format!("\"{label}\" must be at least {min}")));
                }
            }
            if let Some(max) = param.max {
                if n > max {
                    return Err(KitParamError(format!("\"{label}\" must be at most {max}")));
                }
            }
            Ok(Some(render(&serde_json::json!(n))))
        }
        KitParamType::Boolean => {
            let parsed = match value {
                serde_json::Value::Bool(b) => Some(*b),
                serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" => Some(true),
                    "false" | "0" | "no" => Some(false),
                    _ => None,
                },
                _ => None,
            };
            let Some(b) = parsed else {
                return Err(KitParamError(format!("\"{label}\" must be true or false")));
            };
            Ok(Some(b.to_string()))
        }
        KitParamType::Select => {
            if param.options.is_empty() {
                return Err(KitParamError(format!("\"{label}\" declares no options")));
            }
            if !param.options.iter().any(|o| o.value == text) {
                let allowed: Vec<&str> = param.options.iter().map(|o| o.value.as_str()).collect();
                return Err(KitParamError(format!(
                    "\"{label}\" must be one of: {}",
                    allowed.join(", ")
                )));
            }
            Ok(Some(text))
        }
        KitParamType::String | KitParamType::Folder => {
            // Required means "answered with something", so whitespace does not
            // count — a folder of " " would install a kit pointing nowhere.
            if param.required && text.trim().is_empty() {
                return Err(KitParamError(format!("\"{label}\" is required")));
            }
            Ok(Some(text))
        }
    }
}

/// Validate every answer against the declarations.
///
/// Reports **all** problems, not just the first: a form with three empty
/// required fields should light up three fields, not make the user discover
/// them one install at a time.
pub fn resolve_values(
    params: &[KitParam],
    supplied: &serde_json::Map<String, serde_json::Value>,
) -> Result<KitParamValues, KitParamError> {
    let mut out = KitParamValues::new();
    let mut errors: Vec<String> = Vec::new();

    for param in params {
        match resolve_one(param, supplied.get(&param.key)) {
            Ok(Some(text)) => {
                out.insert(
                    param.key.clone(),
                    KitParamValue {
                        text,
                        secret: param.is_secret(),
                    },
                );
            }
            Ok(None) => {}
            Err(e) => errors.push(e.0),
        }
    }

    if !errors.is_empty() {
        return Err(KitParamError(errors.join("; ")));
    }
    Ok(out)
}

/// Every `{{param.<key>}}` occurring in `text`, in order of appearance.
pub fn placeholders_in(text: &str) -> Vec<String> {
    const OPEN: &str = "{{param.";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let Some(end) = after.find("}}") else { break };
        let key = after[..end].trim();
        if KitParam::key_is_valid(key) && !out.iter().any(|k| k == key) {
            out.push(key.to_string());
        }
        rest = &after[end + 2..];
    }
    out
}

/// Replace every `{{param.<key>}}` that has a value. Unknown keys stay verbatim.
pub fn substitute(text: &str, values: &KitParamValues) -> String {
    if !text.contains("{{param.") {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (key, value) in values {
        out = out.replace(&format!("{{{{param.{key}}}}}"), &value.text);
    }
    out
}

/// Substitute through an arbitrary JSON value — used for `mcpServers` and
/// `apps`, which the daemon never installs but does hand back to the client.
/// An MCP server entry whose API key stayed `{{param.apiKey}}` would be
/// installed broken by whoever picks it up next.
pub fn substitute_json(value: &serde_json::Value, values: &KitParamValues) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(substitute(s, values)),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items.iter().map(|v| substitute_json(v, values)).collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                // Keys are substituted too: an env block writes the variable
                // name as often as the value.
                .map(|(k, v)| (substitute(k, values), substitute_json(v, values)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(key: &str, kind: KitParamType) -> KitParam {
        KitParam {
            key: key.into(),
            kind,
            ..Default::default()
        }
    }

    fn answers(raw: &str) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn a_default_is_used_when_nothing_is_supplied() {
        let mut p = param("runs", KitParamType::Number);
        p.default = Some(serde_json::json!(3));
        let out = resolve_values(&[p], &answers("{}")).unwrap();
        assert_eq!(out["runs"].text, "3");
    }

    #[test]
    fn a_supplied_value_beats_the_default() {
        let mut p = param("runs", KitParamType::Number);
        p.default = Some(serde_json::json!(3));
        let out = resolve_values(&[p], &answers(r#"{"runs":7}"#)).unwrap();
        assert_eq!(out["runs"].text, "7");
    }

    #[test]
    fn whole_numbers_render_without_a_trailing_zero() {
        // "*/3.0 * * * *" is not a cron expression, and "run 3.0 times" is not
        // a sentence — a float here corrupts both.
        let p = param("runs", KitParamType::Number);
        let out = resolve_values(&[p], &answers(r#"{"runs":3.0}"#)).unwrap();
        assert_eq!(out["runs"].text, "3");
    }

    #[test]
    fn a_numeric_string_is_accepted() {
        // An HTML number input hands back a string; rejecting it would fail
        // every install driven from the web form.
        let p = param("runs", KitParamType::Number);
        let out = resolve_values(&[p], &answers(r#"{"runs":"12"}"#)).unwrap();
        assert_eq!(out["runs"].text, "12");
    }

    #[test]
    fn numbers_are_bounded_by_min_and_max() {
        let mut p = param("runs", KitParamType::Number);
        p.min = Some(1.0);
        p.max = Some(10.0);
        assert!(resolve_values(&[p.clone()], &answers(r#"{"runs":0}"#)).is_err());
        assert!(resolve_values(&[p.clone()], &answers(r#"{"runs":11}"#)).is_err());
        assert!(resolve_values(&[p], &answers(r#"{"runs":10}"#)).is_ok());
    }

    #[test]
    fn booleans_accept_the_string_forms_a_form_sends() {
        let p = param("notify", KitParamType::Boolean);
        for (raw, want) in [
            (r#"{"notify":true}"#, "true"),
            (r#"{"notify":"true"}"#, "true"),
            (r#"{"notify":"no"}"#, "false"),
            (r#"{"notify":0}"#, ""),
        ] {
            let got = resolve_values(&[p.clone()], &answers(raw));
            if want.is_empty() {
                assert!(got.is_err(), "{raw} should not parse as a boolean");
            } else {
                assert_eq!(got.unwrap()["notify"].text, want, "for {raw}");
            }
        }
    }

    #[test]
    fn a_select_refuses_a_value_outside_its_options() {
        let mut p = param("tier", KitParamType::Select);
        p.options = vec![
            KitParamOption { value: "fast".into(), label: String::new() },
            KitParamOption { value: "deep".into(), label: String::new() },
        ];
        assert!(resolve_values(&[p.clone()], &answers(r#"{"tier":"deep"}"#)).is_ok());
        let err = resolve_values(&[p], &answers(r#"{"tier":"turbo"}"#)).unwrap_err();
        assert!(err.0.contains("fast, deep"), "{}", err.0);
    }

    #[test]
    fn required_rejects_absent_null_and_whitespace() {
        let mut p = param("workdir", KitParamType::Folder);
        p.required = true;
        for raw in ["{}", r#"{"workdir":null}"#, r#"{"workdir":"   "}"#] {
            assert!(
                resolve_values(&[p.clone()], &answers(raw)).is_err(),
                "{raw} should be rejected"
            );
        }
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        // Three empty required fields should light up three fields, not make
        // the user discover them one install at a time.
        let mut a = param("a", KitParamType::String);
        a.required = true;
        let mut b = param("b", KitParamType::String);
        b.required = true;
        let err = resolve_values(&[a, b], &answers("{}")).unwrap_err();
        assert!(err.0.contains("\"a\""), "{}", err.0);
        assert!(err.0.contains("\"b\""), "{}", err.0);
    }

    #[test]
    fn substitution_only_touches_the_param_namespace() {
        // A workflow body legitimately contains its own {{…}} syntax; an
        // un-namespaced replace would corrupt it.
        let mut values = KitParamValues::new();
        values.insert(
            "dir".into(),
            KitParamValue { text: "/tmp/x".into(), secret: false },
        );
        let got = substitute("cd {{param.dir}} && echo {{dir}} {{ item.title }}", &values);
        assert_eq!(got, "cd /tmp/x && echo {{dir}} {{ item.title }}");
    }

    #[test]
    fn an_optional_param_left_blank_substitutes_as_empty() {
        // Never-touched and cleared-to-empty must land the same way: the form
        // sends nothing for one and "" for the other.
        let p = param("note", KitParamType::String);
        let untouched = resolve_values(&[p.clone()], &answers("{}")).unwrap();
        let cleared = resolve_values(&[p], &answers(r#"{"note":""}"#)).unwrap();
        assert_eq!(untouched["note"].text, "");
        assert_eq!(cleared["note"].text, "");

        // …and neither leaves a template artifact behind.
        assert_eq!(substitute("hi {{param.note}}", &untouched), "hi ");
    }

    #[test]
    fn an_unanswered_placeholder_is_left_verbatim() {
        // No param declares `apiKey` — an author's typo. Blanking that would
        // hide the mistake, so it survives as literal text (and `warnings()`
        // reports it). Contrast with the declared-but-blank case above.
        let values = KitParamValues::new();
        assert_eq!(substitute("key={{param.apiKey}}", &values), "key={{param.apiKey}}");
    }

    #[test]
    fn placeholders_are_found_deduped_and_in_order() {
        let found = placeholders_in("{{param.b}} {{param.a}} {{param.b}} {{nope}} {{param.}}");
        assert_eq!(found, vec!["b", "a"]);
    }

    #[test]
    fn json_substitution_covers_keys_and_nested_values() {
        // An env block writes the variable name as often as the value.
        let mut values = KitParamValues::new();
        values.insert("k".into(), KitParamValue { text: "SECRET".into(), secret: true });
        let input = serde_json::json!({
            "env": { "{{param.k}}_TOKEN": ["{{param.k}}", 5, true] }
        });
        let got = substitute_json(&input, &values);
        assert_eq!(got["env"]["SECRET_TOKEN"][0], "SECRET");
        assert_eq!(got["env"]["SECRET_TOKEN"][1], 5);
    }

    #[test]
    fn key_validation_rejects_what_the_placeholder_cannot_express() {
        assert!(KitParam::key_is_valid("api_key-2"));
        assert!(!KitParam::key_is_valid(""));
        assert!(!KitParam::key_is_valid("a.b"));
        assert!(!KitParam::key_is_valid("a b"));
    }
}

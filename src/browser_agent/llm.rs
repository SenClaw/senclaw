//! The LLM's four jobs in the browser loop, each with a strict JSON contract:
//! write a field's value, choose when the decision model is unsure, check a
//! DONE the decision model cannot, and answer the caller's question. Output
//! that breaks the contract is rejected — never typed, never executed.

use serde_json::{json, Value};

use super::encoder::{Encoded, HistoryItem};
use super::ports::Llm;
use super::prompts::{ANSWER, CRITERIA, FALLBACK, TEXT_VALUE, VERIFY};

const MAX_TEXT: usize = 2000;

/// The whole input of a TYPE_TEXT request. A generated value is reused after
/// a stale retry only while this is identical.
pub fn text_context(goal: &str, field: &Value, observation: &Value, history: &[HistoryItem]) -> Value {
    let text: String = observation.get("text").and_then(Value::as_str).unwrap_or_default().chars().take(6000).collect();
    json!({
        "goal": goal,
        "field": {
            "label": field.get("label").cloned().unwrap_or(Value::Null),
            "role": field.get("role").cloned().unwrap_or(Value::Null),
            "value": field.get("value").cloned().unwrap_or(Value::Null),
        },
        "page": { "title": observation.get("title").cloned().unwrap_or(Value::Null), "text": text },
        "recent_actions": history.iter().rev().take(6).rev()
            .map(|h| json!({ "action": h.action, "text": h.text })).collect::<Vec<_>>(),
    })
}

/// Pull the JSON object out of an answer that may be wrapped in a code fence.
fn json_object(raw: &str) -> Option<serde_json::Map<String, Value>> {
    let t = raw.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    serde_json::from_str::<Value>(t).ok()?.as_object().cloned()
}

#[derive(Debug, PartialEq)]
pub enum TextValue {
    Text(String),
    /// The goal does not contain the value: ask the person, never invent it.
    Missing,
}

/// Parse the text helper's answer: exactly `{"text": string}` or `{"text": null}`.
pub fn parse_text_value(raw: &str) -> Result<TextValue, String> {
    let bad = || "the text helper returned no valid field value; nothing typed".to_string();
    let obj = json_object(raw).ok_or_else(bad)?;
    if obj.len() != 1 {
        return Err(bad());
    }
    match obj.get("text") {
        Some(Value::Null) => Ok(TextValue::Missing),
        Some(Value::String(s)) if !s.trim().is_empty() && s.chars().count() <= MAX_TEXT => Ok(TextValue::Text(s.clone())),
        _ => Err(bad()),
    }
}

pub async fn text_value(llm: &dyn Llm, model: Option<&str>, context: &Value) -> Result<TextValue, String> {
    let raw = llm.complete(model, TEXT_VALUE, &context.to_string(), 256).await?;
    parse_text_value(&raw)
}

/// The LLM tier's pick: an offered operation and, when it needs one, an offered target.
pub async fn fallback(
    llm: &dyn Llm,
    model: Option<&str>,
    encoded: &Encoded,
    observation: &Value,
    goal: &str,
    history: &[HistoryItem],
    model_guesses: &Value,
) -> Result<(String, Option<String>, String), String> {
    let text: String = observation.get("text").and_then(Value::as_str).unwrap_or_default().chars().take(4000).collect();
    let elements = serde_json::to_value(&encoded.space.elements).unwrap_or(Value::Null);
    let input = json!({
        "goal": goal,
        "page": { "url": observation.get("url"), "title": observation.get("title"), "text": text },
        "elements": elements,
        "operations": encoded.operations,
        "recent_actions": history.iter().rev().take(10).rev().collect::<Vec<_>>(),
        "decision_model_guesses": model_guesses,
    });
    let raw = llm.complete(model, FALLBACK, &input.to_string(), 300).await?;
    parse_fallback(&raw, encoded)
}

pub fn parse_fallback(raw: &str, encoded: &Encoded) -> Result<(String, Option<String>, String), String> {
    let obj = json_object(raw).ok_or("the fallback model did not answer with JSON")?;
    let operation = obj.get("operation").and_then(Value::as_str).unwrap_or_default().to_string();
    if !encoded.operations.contains(&operation) {
        return Err(format!("the fallback model chose an operation that was not offered: {operation:?}"));
    }
    let reason = obj.get("reason").and_then(Value::as_str).unwrap_or_default().to_string();
    let needs_target = encoded.space.operations_with_targets().any(|op| op == operation);
    if !needs_target {
        return Ok((operation, None, reason));
    }
    let target = match obj.get("target") {
        Some(Value::String(t)) => t.trim_start_matches('[').trim_end_matches(']').to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return Err("the fallback model gave no target".into()),
    };
    if encoded.space.target(&operation, &target).is_none() {
        return Err(format!("the fallback model chose a target that was not offered: {target:?}"));
    }
    Ok((operation, Some(target), reason))
}

/// An LLM check of one criterion, used when no decision model is available.
pub async fn verify(llm: &dyn Llm, model: Option<&str>, criterion: &str, page: &Value) -> Result<bool, String> {
    let input = json!({ "criterion": criterion, "page": page });
    let system = format!("{VERIFY}\nReturn only JSON: {{\"satisfied\": true|false}}.");
    let raw = llm.complete(model, &system, &input.to_string(), 50).await?;
    let obj = json_object(&raw).ok_or("the verifier did not answer with JSON")?;
    obj.get("satisfied").and_then(Value::as_bool).ok_or_else(|| "the verifier gave no verdict".to_string())
}

pub async fn answer(llm: &dyn Llm, model: Option<&str>, question: &str, read: &Value) -> Result<String, String> {
    let input = json!({ "question": question, "page": read });
    llm.complete(model, ANSWER, &input.to_string(), 700).await.map(|s| s.trim().to_string())
}

/// Checkable completion criteria for a goal (when the caller gave none).
pub async fn criteria(llm: &dyn Llm, model: Option<&str>, goal: &str) -> Vec<String> {
    let parsed = match llm.complete(model, CRITERIA, goal, 300).await {
        Ok(raw) => json_object(&raw).and_then(|o| o.get("criteria").cloned()),
        Err(_) => None,
    };
    let list: Vec<String> = parsed
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|c| c.as_str().map(|s| s.trim().to_string()))
        .filter(|c| !c.is_empty())
        .take(5)
        .collect();
    if list.is_empty() {
        vec![goal.to_string()]
    } else {
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_agent::encoder::{encode, Profile};

    #[test]
    fn text_value_contract() {
        assert_eq!(parse_text_value(r#"{"text":"Zurich"}"#).unwrap(), TextValue::Text("Zurich".into()));
        assert_eq!(parse_text_value("```json\n{\"text\": \"London\"}\n```").unwrap(), TextValue::Text("London".into()));
        assert_eq!(parse_text_value(r#"{"text":null}"#).unwrap(), TextValue::Missing);
        for bad in ["Thinking: Zurich", r#"{"text":"Zurich","extra":true}"#, r#"{"text":123}"#, r#"{"text":"  "}"#, r#"["Zurich"]"#] {
            assert!(parse_text_value(bad).is_err(), "{bad} must be rejected");
        }
        let long = format!(r#"{{"text":"{}"}}"#, "x".repeat(MAX_TEXT + 1));
        assert!(parse_text_value(&long).is_err());
    }

    #[test]
    fn fallback_names_only_what_was_offered() {
        let page = json!({
            "url": "https://a.test/", "title": "A", "text": "x",
            "actions": [
                {"id": "e1", "node": 1, "kind": "fill", "role": "textbox", "label": "Search", "value": ""},
                {"id": "e2", "node": 2, "kind": "click", "role": "button", "label": "Go", "value": ""},
                {"id": "wait", "kind": "wait", "label": "Wait"}
            ]
        });
        let encoded = encode(&page, "g", &[], Profile::JevFull, None, None);
        assert_eq!(parse_fallback(r#"{"operation":"CLICK","target":"2","reason":"go"}"#, &encoded).unwrap().1.as_deref(), Some("2"));
        assert_eq!(parse_fallback(r#"{"operation":"CLICK","target":2}"#, &encoded).unwrap().1.as_deref(), Some("2"));
        assert_eq!(parse_fallback(r#"{"operation":"WAIT","target":null}"#, &encoded).unwrap().0, "WAIT");
        assert!(parse_fallback(r#"{"operation":"CLICK","target":"9"}"#, &encoded).is_err());
        assert!(parse_fallback(r#"{"operation":"EVAL_JS","target":null}"#, &encoded).is_err());
        assert!(parse_fallback(r#"{"operation":"TYPE_TEXT","target":"2"}"#, &encoded).is_err(), "2 cannot be typed into");
        assert!(parse_fallback("click the button", &encoded).is_err());
    }
}

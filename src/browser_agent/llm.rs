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

/// Pull the JSON object out of an answer. Small local models wrap it in a
/// code fence, prefix a `<think>` block, or explain themselves first; the
/// object that *ends* the answer is the answer. What it may contain is still
/// checked strictly by each caller.
fn json_object(raw: &str) -> Option<serde_json::Map<String, Value>> {
    let mut t = raw.trim();
    if let Some(end) = t.find("</think>") {
        t = t[end + "</think>".len()..].trim();
    }
    let t = t.strip_suffix("```").unwrap_or(t).trim_end();
    let whole = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(whole.trim()) {
        return Some(o);
    }
    // The last object that runs to the end of the answer.
    for (start, _) in t.match_indices('{').collect::<Vec<_>>().into_iter().rev() {
        if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(&t[start..]) {
            return Some(o);
        }
    }
    None
}

/// One more try when an answer breaks its contract: local models sample, and
/// the second answer is usually well-formed. Never more — a model that cannot
/// follow the contract gets the step taken from it.
async fn complete_twice<T>(
    llm: &dyn Llm,
    model: Option<&str>,
    system: &str,
    user: &str,
    max_tokens: u32,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<T, String> {
    let first = parse(&llm.complete(model, system, user, max_tokens).await?);
    match first {
        Ok(v) => Ok(v),
        Err(_) => parse(&llm.complete(model, system, user, max_tokens).await?),
    }
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
    complete_twice(llm, model, TEXT_VALUE, &context.to_string(), 256, parse_text_value).await
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
) -> Result<(String, Option<String>), String> {
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
    complete_twice(llm, model, FALLBACK, &input.to_string(), 200, |raw| parse_fallback(raw, encoded)).await
}

pub fn parse_fallback(raw: &str, encoded: &Encoded) -> Result<(String, Option<String>), String> {
    let obj = json_object(raw).ok_or("the fallback model did not answer with JSON")?;
    let operation = obj.get("operation").and_then(Value::as_str).unwrap_or_default().to_string();
    if !encoded.operations.contains(&operation) {
        return Err(format!("the fallback model chose an operation that was not offered: {operation:?}"));
    }
    let needs_target = encoded.space.operations_with_targets().any(|op| op == operation);
    if !needs_target {
        return Ok((operation, None));
    }
    let target = match obj.get("target") {
        Some(Value::String(t)) => t.trim_start_matches('[').trim_end_matches(']').to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return Err("the fallback model gave no target".into()),
    };
    if encoded.space.target(&operation, &target).is_none() {
        return Err(format!("the fallback model chose a target that was not offered: {target:?}"));
    }
    Ok((operation, Some(target)))
}

/// An LLM check of the criteria, used when no decision model is available:
/// one verdict per criterion, in order, from a single call.
pub async fn verify(llm: &dyn Llm, model: Option<&str>, criteria: &[String], page: &Value) -> Result<Vec<bool>, String> {
    let input = json!({ "criteria": criteria, "page": page });
    let system = format!(
        "{VERIFY}\nReturn only JSON: {{\"satisfied\": [true|false, ...]}} — one verdict per criterion, in the order given."
    );
    let max_tokens = 40 + 8 * criteria.len() as u32;
    complete_twice(llm, model, &system, &input.to_string(), max_tokens, |raw| parse_verdicts(raw, criteria.len())).await
}

/// Exactly one boolean per criterion; anything else is no verdict at all.
pub fn parse_verdicts(raw: &str, expected: usize) -> Result<Vec<bool>, String> {
    let obj = json_object(raw).ok_or("the verifier did not answer with JSON")?;
    let verdicts: Option<Vec<bool>> = obj.get("satisfied").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_bool).collect());
    match verdicts {
        Some(v) if v.len() == expected => Ok(v),
        _ => Err("the verifier gave no verdict for every criterion".to_string()),
    }
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
        // What small local models add around the object; the object ending the answer counts.
        assert_eq!(parse_text_value("<think>The goal says London.</think>\n{\"text\": \"London\"}").unwrap(), TextValue::Text("London".into()));
        assert_eq!(parse_text_value("The field is the destination, so:\n```json\n{\"text\": \"London\"}\n```").unwrap(), TextValue::Text("London".into()));
        assert!(parse_text_value("{\"text\": \"London\"} and then I would also type Paris").is_err(), "the object must end the answer");
    }

    #[tokio::test]
    async fn an_off_contract_answer_gets_one_retry() {
        struct Answers(std::sync::Mutex<Vec<&'static str>>);
        #[async_trait::async_trait]
        impl Llm for Answers {
            async fn complete(&self, _m: Option<&str>, _s: &str, _u: &str, _t: u32) -> Result<String, String> {
                Ok(self.0.lock().unwrap().remove(0).to_string())
            }
        }
        let ctx = json!({});
        let llm = Answers(std::sync::Mutex::new(vec!["I think the value is London.", r#"{"text":"London"}"#]));
        assert_eq!(text_value(&llm, None, &ctx).await.unwrap(), TextValue::Text("London".into()));
        let llm = Answers(std::sync::Mutex::new(vec!["London", "London again", r#"{"text":"London"}"#]));
        assert!(text_value(&llm, None, &ctx).await.is_err(), "never a third try");
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
        assert_eq!(parse_fallback(r#"{"operation":"CLICK","target":"2"}"#, &encoded).unwrap().1.as_deref(), Some("2"));
        assert_eq!(parse_fallback(r#"{"operation":"CLICK","target":"2","reason":"go"}"#, &encoded).unwrap().1.as_deref(), Some("2"), "an explanation is tolerated");
        assert_eq!(parse_fallback(r#"{"operation":"CLICK","target":2}"#, &encoded).unwrap().1.as_deref(), Some("2"));
        assert_eq!(parse_fallback(r#"{"operation":"WAIT","target":null}"#, &encoded).unwrap().0, "WAIT");
        assert!(parse_fallback(r#"{"operation":"CLICK","target":"9"}"#, &encoded).is_err());
        assert!(parse_fallback(r#"{"operation":"EVAL_JS","target":null}"#, &encoded).is_err());
        assert!(parse_fallback(r#"{"operation":"TYPE_TEXT","target":"2"}"#, &encoded).is_err(), "2 cannot be typed into");
        assert!(parse_fallback("click the button", &encoded).is_err());
    }

    #[test]
    fn a_verdict_is_needed_for_every_criterion() {
        assert_eq!(parse_verdicts(r#"{"satisfied":[true,false]}"#, 2).unwrap(), vec![true, false]);
        assert_eq!(parse_verdicts("```json\n{\"satisfied\": [true]}\n```", 1).unwrap(), vec![true]);
        for bad in [r#"{"satisfied":[true]}"#, r#"{"satisfied":true}"#, r#"{"satisfied":[true,"yes"]}"#, "both hold"] {
            assert!(parse_verdicts(bad, 2).is_err(), "{bad} must not count as two verdicts");
        }
    }
}

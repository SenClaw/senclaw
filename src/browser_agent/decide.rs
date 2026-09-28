//! Decision-model answers → one validated step with a confidence band.
//!
//! Validation is jev-ultrafast's `validate_choice`: the choice must be offered,
//! every offered id must carry a probability in [0, 1] summing to ~1, and the
//! choice must be the argmax. Anything else is rejected — no action executes
//! on a malformed answer. The step's confidence is p(operation) × p(target):
//! two independent heads both have to be sure.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::encoder::Encoded;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bands {
    pub act: f64,
    pub fallback: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Band {
    /// Confident: act.
    Act,
    /// Unsure: the LLM tier chooses on the same table.
    Fallback,
    /// Too unsure to guess: the LLM tier, and if it cannot, the person.
    Review,
}

impl Bands {
    pub fn classify(&self, confidence: f64) -> Band {
        if confidence >= self.act {
            Band::Act
        } else if confidence >= self.fallback {
            Band::Fallback
        } else {
            Band::Review
        }
    }
}

#[derive(Debug, Clone)]
pub struct Choice {
    pub choice: String,
    pub probabilities: Map<String, Value>,
    /// The chosen option's probability.
    pub p: f64,
}

/// `validate_choice` from jev-ultrafast.
pub fn validate_choice(answer: &Value, ids: &[String]) -> Result<Choice, String> {
    let bad = || "invalid decision answer; no action executed".to_string();
    let choice = answer.get("choice").and_then(Value::as_str).ok_or_else(bad)?.to_string();
    let probabilities = answer.get("probabilities").and_then(Value::as_object).ok_or_else(bad)?.clone();
    let confidence = answer.get("confidence").and_then(Value::as_f64).ok_or_else(bad)?;
    if !ids.iter().any(|id| *id == choice) {
        return Err(bad());
    }
    if probabilities.len() != ids.len() || !ids.iter().all(|id| probabilities.contains_key(id)) {
        return Err(bad());
    }
    let mut numbers: Vec<f64> = Vec::with_capacity(probabilities.len() + 1);
    for v in probabilities.values() {
        numbers.push(v.as_f64().ok_or_else(bad)?);
    }
    numbers.push(confidence);
    if numbers.iter().any(|n| !n.is_finite() || *n < 0.0 || *n > 1.0) {
        return Err(bad());
    }
    let sum: f64 = numbers[..numbers.len() - 1].iter().sum();
    if (sum - 1.0).abs() >= 0.02 {
        return Err(bad());
    }
    let p = probabilities[&choice].as_f64().ok_or_else(bad)?;
    let max = numbers[..numbers.len() - 1].iter().cloned().fold(f64::MIN, f64::max);
    if p < max - 1e-6 {
        return Err(bad());
    }
    Ok(Choice { choice, probabilities, p })
}

fn top(probabilities: &Map<String, Value>, n: usize) -> Vec<(String, f64)> {
    let mut all: Vec<(String, f64)> = probabilities.iter().filter_map(|(k, v)| Some((k.clone(), v.as_f64()?))).collect();
    all.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    all.truncate(n);
    all
}

/// One resolved step.
#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub operation: String,
    /// Element index for CLICK / TYPE_TEXT / SELECT.
    pub target: Option<String>,
    /// The runtime action to execute (`e7`, `scroll_down`, …), or `DONE` / `BLOCKED`.
    pub action_id: String,
    pub label: String,
    pub kind: String,
    pub p_operation: f64,
    pub p_target: Option<f64>,
    pub confidence: f64,
    pub band: Band,
    pub top_operations: Vec<(String, f64)>,
    pub top_targets: Vec<(String, f64)>,
    #[serde(skip)]
    pub action: Value,
}

/// Validate the answers to one encoded request and name the action to run.
pub fn resolve(answers: &Value, encoded: &Encoded, bands: Bands) -> Result<Step, String> {
    let op_answer = validate_choice(answers.get("operation").unwrap_or(&Value::Null), &encoded.operations)?;
    let operation = op_answer.choice.clone();
    let top_operations = top(&op_answer.probabilities, 3);

    if let Some((_, candidates)) = encoded.space.targets.iter().find(|(op, _)| *op == operation) {
        let head = format!("{}_target", operation.to_lowercase());
        let ids: Vec<String> = candidates.iter().map(|t| t.index.clone()).collect();
        // Only the head the operation selected is validated and used; the
        // others were speculative.
        let t_answer = validate_choice(answers.get(&head).unwrap_or(&Value::Null), &ids)?;
        let target = encoded.space.target(&operation, &t_answer.choice).expect("validated target");
        let confidence = op_answer.p * t_answer.p;
        return Ok(Step {
            operation,
            target: Some(t_answer.choice.clone()),
            action_id: target.action.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
            label: target.action.get("label").and_then(Value::as_str).unwrap_or_default().to_string(),
            kind: target.action.get("kind").and_then(Value::as_str).unwrap_or_default().to_string(),
            p_operation: op_answer.p,
            p_target: Some(t_answer.p),
            confidence,
            band: bands.classify(confidence),
            top_operations,
            top_targets: top(&t_answer.probabilities, 3),
            action: target.action.clone(),
        });
    }
    let (action_id, label, kind, action) = match encoded.space.control(&operation) {
        Some(a) => (
            a.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
            a.get("label").and_then(Value::as_str).unwrap_or_default().to_string(),
            a.get("kind").and_then(Value::as_str).unwrap_or_default().to_string(),
            a.clone(),
        ),
        None => (operation.clone(), operation.clone(), operation.to_lowercase(), Value::Null),
    };
    Ok(Step {
        operation,
        target: None,
        action_id,
        label,
        kind,
        p_operation: op_answer.p,
        p_target: None,
        confidence: op_answer.p,
        band: bands.classify(op_answer.p),
        top_operations,
        top_targets: Vec::new(),
        action,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_agent::encoder::{encode, Profile};
    use serde_json::json;

    fn choice(ids: &[&str], selected: &str) -> Value {
        let probs: Map<String, Value> = ids.iter().map(|i| (i.to_string(), json!(if *i == selected { 1.0 } else { 0.0 }))).collect();
        json!({ "choice": selected, "confidence": 1.0, "probabilities": probs })
    }

    #[test]
    fn invalid_choices_are_rejected() {
        let ids = vec!["a".to_string(), "b".to_string()];
        assert!(validate_choice(&choice(&["a", "b"], "a"), &ids).is_ok());
        let mutate = |f: &dyn Fn(&mut Value)| {
            let mut a = choice(&["a", "b"], "a");
            f(&mut a);
            validate_choice(&a, &ids).is_err()
        };
        assert!(mutate(&|a| a["choice"] = json!("invented")), "unknown choice");
        assert!(mutate(&|a| a["probabilities"]["a"] = json!("NaN")), "non-number");
        assert!(mutate(&|a| {
            a["probabilities"].as_object_mut().unwrap().remove("b");
        }), "missing id");
        assert!(mutate(&|a| a["probabilities"]["b"] = json!(-1)), "negative");
        assert!(mutate(&|a| a["choice"] = json!("b")), "choice not the argmax");
        assert!(mutate(&|a| a["confidence"] = json!(5)), "confidence out of range");
        assert!(mutate(&|a| a["probabilities"] = json!({"a": 0.5, "b": 0.2})), "does not sum to 1");
    }

    fn page() -> Value {
        json!({
            "url": "https://a.test/", "title": "A", "text": "x",
            "actions": [
                {"id": "e1", "node": 1, "kind": "fill", "role": "textbox", "label": "Search", "value": ""},
                {"id": "e2", "node": 1, "kind": "click", "role": "textbox", "label": "Open Search", "value": ""},
                {"id": "e3", "node": 2, "kind": "click", "role": "button", "label": "Go", "value": ""},
                {"id": "wait", "kind": "wait", "label": "Wait"}
            ]
        })
    }

    fn answers(op: &str, p_op: f64, head: Option<(&str, &str, f64)>) -> Value {
        let ops = ["TYPE_TEXT", "CLICK", "WAIT", "DONE", "BLOCKED"];
        let rest = (1.0 - p_op) / (ops.len() - 1) as f64;
        let probs: Map<String, Value> = ops.iter().map(|o| (o.to_string(), json!(if *o == op { p_op } else { rest }))).collect();
        let mut out = json!({ "operation": { "choice": op, "confidence": p_op, "probabilities": probs } });
        if let Some((name, pick, p)) = head {
            let ids: Vec<&str> = if name == "click_target" { vec!["1", "2"] } else { vec!["1"] };
            let rest = if ids.len() > 1 { (1.0 - p) / (ids.len() - 1) as f64 } else { 0.0 };
            let probs: Map<String, Value> = ids.iter().map(|i| (i.to_string(), json!(if *i == pick { p } else { rest }))).collect();
            out[name] = json!({ "choice": pick, "confidence": p, "probabilities": probs });
        }
        out
    }

    #[test]
    fn bands_use_joint_confidence() {
        let encoded = encode(&page(), "find", &[], Profile::JevFull, None, None);
        let bands = Bands { act: 0.6, fallback: 0.3 };
        let step = resolve(&answers("CLICK", 0.9, Some(("click_target", "2", 0.95))), &encoded, bands).unwrap();
        assert_eq!(step.action_id, "e3");
        assert!((step.confidence - 0.855).abs() < 1e-9);
        assert_eq!(step.band, Band::Act);
        let step = resolve(&answers("CLICK", 0.9, Some(("click_target", "2", 0.5))), &encoded, bands).unwrap();
        assert_eq!(step.band, Band::Fallback, "0.9 × 0.5 = 0.45");
        let step = resolve(&answers("CLICK", 0.5, Some(("click_target", "2", 0.5))), &encoded, bands).unwrap();
        assert_eq!(step.band, Band::Review, "0.25 is below fallback");
        let step = resolve(&answers("WAIT", 0.8, None), &encoded, bands).unwrap();
        assert_eq!(step.action_id, "wait");
        assert_eq!(step.band, Band::Act);
        let step = resolve(&answers("DONE", 0.99, None), &encoded, bands).unwrap();
        assert_eq!(step.action_id, "DONE");
        // TYPE_TEXT resolves through its own head only.
        let step = resolve(&answers("TYPE_TEXT", 0.9, Some(("type_text_target", "1", 1.0))), &encoded, bands).unwrap();
        assert_eq!(step.action_id, "e1");
        // A CLICK whose chosen head is malformed never executes.
        let mut bad = answers("CLICK", 0.9, Some(("click_target", "2", 0.95)));
        bad["click_target"]["choice"] = json!("999");
        assert!(resolve(&bad, &encoded, bands).is_err());
    }
}

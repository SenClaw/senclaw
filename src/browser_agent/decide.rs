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
use super::ports::Decider;
use crate::decision::json::Json;
use crate::decision::types::AskRequest;

/// The widest choice a format-v5 checkpoint is asked at once. Its head reads
/// a question's options inside one 768-token budget and cuts every option
/// shorter as their number grows; a busy page offers well over a hundred.
pub const MAX_OPTIONS_V5: usize = 60;

/// Ask `request` with every choice wider than `max` split the way
/// laya-browser v19s is served (`predict_chunked` in the model card's
/// `laya_browser.py`): the options are dealt into interleaved chunks, asked as
/// questions of their own in the same pass, and the chunk winners compete in
/// a second pass. p(option) = p(its chunk's winner in the second pass) ×
/// p(option within its chunk), normalized.
pub async fn ask_chunked(decider: &dyn Decider, request: &AskRequest, max: usize) -> Result<Value, String> {
    let Json::Object(questions) = &request.questions else { return decider.ask(request).await };
    let mut first: Vec<(String, Json)> = Vec::new();
    let mut plan: Vec<(&str, &Json, Vec<Vec<String>>)> = Vec::new();
    for (id, question) in questions {
        let keys = choice_keys(question);
        if keys.len() <= max {
            first.push((id.clone(), question.clone()));
            continue;
        }
        let n = keys.len().div_ceil(max);
        let chunks: Vec<Vec<String>> = (0..n).map(|i| keys.iter().skip(i).step_by(n).cloned().collect()).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            first.push((format!("{id}__chunk{i}"), with_options(question, chunk)));
        }
        plan.push((id, question, chunks));
    }
    if plan.is_empty() {
        return decider.ask(request).await;
    }

    let mut answers = decider.ask(&AskRequest { questions: Json::Object(first), ..request.clone() }).await?;
    let missing = |what: &str| format!("the decision runtime did not answer {what}");
    let mut finals: Vec<(String, Json)> = Vec::new();
    let mut by_chunk: Vec<Vec<Value>> = Vec::new();
    for (id, question, chunks) in &plan {
        let mut winners = Vec::new();
        let mut chunk_answers = Vec::new();
        for i in 0..chunks.len() {
            let key = format!("{id}__chunk{i}");
            let answer = answers.as_object_mut().and_then(|a| a.remove(&key)).ok_or_else(|| missing(&key))?;
            winners.push(answer.get("choice").and_then(Value::as_str).ok_or_else(|| missing(&key))?.to_string());
            chunk_answers.push(answer);
        }
        finals.push((id.to_string(), with_options(question, &winners)));
        by_chunk.push(chunk_answers);
    }
    let second = decider.ask(&AskRequest { questions: Json::Object(finals), ..request.clone() }).await?;

    for ((id, _, chunks), chunk_answers) in plan.iter().zip(by_chunk) {
        let last = second.get(*id).ok_or_else(|| missing(id))?;
        let mut probabilities: Vec<(String, f64)> = Vec::new();
        for (answer, chunk) in chunk_answers.iter().zip(chunks) {
            let winner = answer.get("choice").and_then(Value::as_str).unwrap_or_default();
            let p_winner = probability(last, winner);
            for key in chunk {
                probabilities.push((key.clone(), p_winner * probability(answer, key)));
            }
        }
        let total = probabilities.iter().map(|(_, p)| p).sum::<f64>();
        let total = if total == 0.0 { 1.0 } else { total };
        for (_, p) in probabilities.iter_mut() {
            *p /= total;
        }
        // The first of equals wins, as Python's `max` picks it.
        let mut best = 0;
        for i in 1..probabilities.len() {
            if probabilities[i].1 > probabilities[best].1 {
                best = i;
            }
        }
        let choice = probabilities[best].0.clone();
        let probabilities: Map<String, Value> = probabilities.into_iter().map(|(k, p)| (k, Value::from(p))).collect();
        let answer = serde_json::json!({
            "type": "choice",
            "choice": choice,
            "probabilities": probabilities,
            "confidence": last.get("confidence").cloned().unwrap_or(Value::Null),
        });
        if let Some(all) = answers.as_object_mut() {
            all.insert(id.to_string(), answer);
        }
    }
    Ok(answers)
}

/// The option keys of a choice question, in order; none for anything else.
fn choice_keys(question: &Json) -> Vec<String> {
    match (question.get("type").and_then(Json::as_str), question.get("criteria")) {
        (Some("choice"), Some(Json::Object(options))) => options.iter().map(|(k, _)| k.clone()).collect(),
        _ => Vec::new(),
    }
}

/// `question` asking only `keys`, in that order; everything else as it was.
fn with_options(question: &Json, keys: &[String]) -> Json {
    let Json::Object(fields) = question else { return question.clone() };
    let fields = fields
        .iter()
        .map(|(k, v)| match (k.as_str(), v) {
            ("criteria", Json::Object(options)) => {
                let kept = keys.iter().filter_map(|key| options.iter().find(|(o, _)| o == key).cloned()).collect();
                (k.clone(), Json::Object(kept))
            }
            _ => (k.clone(), v.clone()),
        })
        .collect();
    Json::Object(fields)
}

fn probability(answer: &Value, key: &str) -> f64 {
    answer.get("probabilities").and_then(|p| p.get(key)).and_then(Value::as_f64).unwrap_or(0.0)
}

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
    let operation_answer = encoded.loop_names(answers.get("operation").unwrap_or(&Value::Null));
    let op_answer = validate_choice(&operation_answer, &encoded.operations)?;
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

    /// The fixture generator's stand-in for the model: option i of question
    /// `id` weighs ((7i + len(id)) mod 11) + 1. It records what each pass asked.
    #[derive(Default)]
    struct StandIn {
        passes: std::sync::Mutex<Vec<std::collections::BTreeMap<String, Vec<String>>>>,
    }

    #[async_trait::async_trait]
    impl Decider for StandIn {
        async fn ask(&self, request: &AskRequest) -> Result<Value, String> {
            let Json::Object(questions) = &request.questions else { return Err("no questions".into()) };
            let mut asked = std::collections::BTreeMap::new();
            let mut answers = Map::new();
            for (id, question) in questions {
                let keys = choice_keys(question);
                let weights: Vec<f64> = (0..keys.len()).map(|i| ((7 * i + id.chars().count()) % 11 + 1) as f64).collect();
                let total: f64 = weights.iter().sum();
                let mut best = 0;
                for i in 1..weights.len() {
                    if weights[i] > weights[best] {
                        best = i;
                    }
                }
                let probabilities: Map<String, Value> = keys.iter().zip(&weights).map(|(k, w)| (k.clone(), json!(w / total))).collect();
                answers.insert(
                    id.clone(),
                    json!({"type": "choice", "choice": keys[best], "probabilities": probabilities, "confidence": weights[best] / total}),
                );
                asked.insert(id.clone(), keys);
            }
            self.passes.lock().unwrap().push(asked);
            Ok(Value::Object(answers))
        }
    }

    #[tokio::test]
    async fn a_wide_choice_is_asked_in_chunks_whose_winners_compete() {
        let fixture: Value = serde_json::from_str(include_str!("testdata/laya_v5.json")).unwrap();
        let case = fixture["cases"].as_array().unwrap().iter().find(|c| c["name"] == "wide").unwrap();
        let history: Vec<crate::browser_agent::encoder::HistoryItem> = serde_json::from_value(case["history"].clone()).unwrap();
        let encoded = encode(&case["page"], case["goal"].as_str().unwrap(), &history, Profile::LayaV5, None, None);
        let want = &case["chunked"];
        assert_eq!(want["maxopt"], json!(MAX_OPTIONS_V5));

        let model = StandIn::default();
        let answers = ask_chunked(&model, &encoded.request, MAX_OPTIONS_V5).await.unwrap();

        let passes = model.passes.lock().unwrap().clone();
        let expected: Vec<std::collections::BTreeMap<String, Vec<String>>> = serde_json::from_value(want["passes"].clone()).unwrap();
        assert_eq!(passes, expected, "the same questions, chunked the same way, in two passes");
        for (id, answer) in want["answers"].as_object().unwrap() {
            assert_eq!(answers[id]["choice"], answer["choice"], "{id}");
            let got = answers[id]["probabilities"].as_object().unwrap();
            for (k, p) in answer["probabilities"].as_object().unwrap() {
                assert!((got[k].as_f64().unwrap() - p.as_f64().unwrap()).abs() < 1e-12, "{id}/{k}");
            }
            assert_eq!(got.len(), answer["probabilities"].as_object().unwrap().len());
        }
        // Narrow requests are asked once, as they are.
        let narrow = encode(&page(), "find", &[], Profile::LayaV5, None, None);
        let model = StandIn::default();
        ask_chunked(&model, &narrow.request, MAX_OPTIONS_V5).await.unwrap();
        assert_eq!(model.passes.lock().unwrap().len(), 1);
    }
}

//! `reasoning_traces`: step-by-step solutions, judged and improved until they
//! pass.
//!
//! After CAMEL's self-improving CoT pipeline, with its defects fixed:
//! - the final answer is read from an explicit last line `Answer: …`, or from
//!   a `\boxed{…}` with nested braces handled (CAMEL's regex reads
//!   `\boxed{\frac{1}{2}}` as `\frac{1`, so ½ and ⅓ compare equal);
//! - the score threshold means one thing: **every** judged dimension must
//!   reach it (CAMEL documents an average and implements a minimum, and an
//!   integer threshold silently turns the gate off);
//! - with a reference answer, the trace's answer must match it, checked in
//!   code — the model is told only that it was wrong, never the answer, so a
//!   kept trace is a solution and not a rationalisation. The judge does not
//!   see the reference either (it would put it in its feedback, which goes
//!   back to the model), and the reference is struck out of the feedback in
//!   case the judge works it out itself;
//! - an unreadable judge reply rejects the problem (`judge_check_error`),
//!   never passes it.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::common::{self, checked_call, verdict_text};
use super::generator::{AcceptLoop, Stop};
use super::jobs::{JobCtx, Outcome};
use super::llm::{call, CallError, Llm};

pub const MAX_PROBLEMS: usize = 2000;
const DEFAULT_ITERATIONS: u32 = 3;
const MAX_ITERATIONS: u32 = 5;
const DEFAULT_THRESHOLD: f64 = 0.7;

/// A problem, with its known answer when there is one. Accepts a bare
/// string, or an object (`problem` / `question`, `answer` / `final_answer`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(from = "ProblemInput")]
pub struct Problem {
    pub problem: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ProblemInput {
    Plain(String),
    Full {
        #[serde(alias = "question")]
        problem: String,
        #[serde(default, alias = "final_answer")]
        answer: Option<serde_json::Value>,
    },
}

impl From<ProblemInput> for Problem {
    fn from(p: ProblemInput) -> Problem {
        match p {
            ProblemInput::Plain(problem) => Problem { problem: problem.trim().to_string(), answer: None },
            ProblemInput::Full { problem, answer } => Problem {
                problem: problem.trim().to_string(),
                // Answers arrive as numbers as often as strings.
                answer: answer
                    .map(|a| match a {
                        serde_json::Value::String(s) => s,
                        other => other.to_string(),
                    })
                    .map(|a| a.trim().to_string())
                    .filter(|a| !a.is_empty()),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Params {
    #[serde(default)]
    pub problems: Vec<Problem>,
    /// Problems read from a dataset on disk instead of `problems`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problems_from: Option<super::from_dataset::DatasetRef>,
    /// Traces wanted; default = every problem.
    #[serde(default)]
    pub target: Option<u32>,
    /// Improvement rounds after the first attempt.
    #[serde(default)]
    pub max_iterations: Option<u32>,
    /// Every judged dimension (correctness, clarity, completeness) must reach this, 0–1.
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub max_llm_calls: Option<u32>,
    #[serde(default)]
    pub profile: Option<String>,
}

impl Params {
    pub fn validate(&self) -> Result<(), String> {
        let n = self.problems.iter().filter(|p| !p.problem.is_empty()).count();
        if n == 0 {
            return Err("problems is empty — give at least one problem to solve".into());
        }
        if n > MAX_PROBLEMS {
            return Err(format!("at most {MAX_PROBLEMS} problems per run"));
        }
        if let Some(t) = self.target {
            if t == 0 || t as usize > n {
                return Err(format!("target must be between 1 and the number of problems ({n})"));
            }
        }
        if let Some(th) = self.threshold {
            if !(0.0..=1.0).contains(&th) {
                return Err("threshold must be between 0 and 1".into());
            }
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
struct Scores {
    correctness: f64,
    clarity: f64,
    completeness: f64,
    #[serde(default)]
    feedback: String,
}

impl Scores {
    fn passes(&self, threshold: f64) -> bool {
        [self.correctness, self.clarity, self.completeness].iter().all(|s| *s >= threshold)
    }
}

const SOLVE_SYSTEM: &str = "You solve problems step by step, showing your reasoning clearly. End with a \
final line of exactly this form: Answer: <final answer>";

const IMPROVE_SYSTEM: &str = "You improve a step-by-step solution using a reviewer's feedback. Write the \
complete improved solution, not a list of changes. End with a final line of exactly this form: \
Answer: <final answer>";

const JUDGE_SYSTEM: &str = "You review step-by-step solutions. Score each from 0 to 1 on correctness \
(is the reasoning valid and the answer right), clarity, and completeness (no skipped steps), and say \
briefly what to fix. Reply with ONLY a JSON object, no prose and no code fence: {\"correctness\": 0.0, \
\"clarity\": 0.0, \"completeness\": 0.0, \"feedback\": \"...\"}";

pub async fn run(params: Params, llm: Arc<dyn Llm>, ctx: &mut JobCtx) -> Result<Outcome, String> {
    params.validate()?;
    let problems: Vec<Problem> = params.problems.iter().filter(|p| !p.problem.is_empty()).cloned().collect();
    let target = params.target.unwrap_or(problems.len() as u32);
    let iterations = params.max_iterations.unwrap_or(DEFAULT_ITERATIONS).min(MAX_ITERATIONS);
    let threshold = params.threshold.unwrap_or(DEFAULT_THRESHOLD);
    // Expected cost per problem: a solve and a judge, plus about one
    // improvement round. A caller's own cap always wins.
    let max_calls = match params.max_llm_calls {
        Some(n) => n.clamp(1, common::MAX_LLM_CALLS),
        None => (target * 4).clamp(10, common::MAX_LLM_CALLS),
    };
    let lang = common::language_rule(params.language.as_deref(), "Write in the language of the problem.");
    let mut l = AcceptLoop::new(target, max_calls);
    let mut last_error = None;
    ctx.set_target(target, l.max_llm_calls);

    let mut queue = problems.into_iter();
    let stop = loop {
        if ctx.cancel.is_cancelled() {
            ctx.publish(&l);
            return Ok(Outcome::Cancelled);
        }
        if let Some(stop) = l.stop() {
            break stop;
        }
        if !l.can_spend(2) {
            break Stop::BudgetSpent;
        }
        let Some(problem) = queue.next() else {
            break Stop::Done;
        };
        l.attempts += 1;

        let mut trace = String::new();
        let mut feedback = String::new();
        let mut kept: Option<(String, String, Scores, u32)> = None;
        let mut reject = "not_improved";
        for round in 0..=iterations {
            if !l.can_spend(2) {
                reject = "budget";
                break;
            }
            let (system, user) = if round == 0 {
                (SOLVE_SYSTEM, format!("Problem:\n{}\n\n{lang}", problem.problem))
            } else {
                (
                    IMPROVE_SYSTEM,
                    format!(
                        "Problem:\n{}\n\nPrevious solution:\n{trace}\n\nFeedback:\n{feedback}\n\n{lang}",
                        problem.problem
                    ),
                )
            };
            match call(llm.as_ref(), &ctx.cancel, system, &user, 2048).await {
                Ok(r) => {
                    l.record_call(&r);
                    ctx.set_model(&r.model);
                    trace = r.text.trim().to_string();
                }
                Err(CallError::Cancelled) => {
                    ctx.publish(&l);
                    return Ok(Outcome::Cancelled);
                }
                Err(CallError::Failed(v)) => {
                    last_error = Some(verdict_text(&v));
                    // Already counted as `llm_error`; nothing more to add.
                    l.record_call_error();
                    reject = "";
                    break;
                }
            }
            let answer = final_answer(&trace);
            let wrong = match (&answer, &problem.answer) {
                (None, _) => Some("The solution has no final line of the form `Answer: <final answer>`."),
                (Some(a), Some(want)) if !same_answer(a, want) => {
                    Some("The final answer is wrong. Find the mistake in the reasoning.")
                }
                _ => None,
            };

            // The judge rates the reasoning; whether the final answer is right
            // is decided above, in code.
            let judge_user = format!("Problem:\n{}\n\nSolution:\n{trace}", problem.problem);
            let scores = match checked_call(llm.as_ref(), ctx, &mut l, JUDGE_SYSTEM, &judge_user, 512 + common::THINKING_ROOM, |s: &Scores| {
                [s.correctness, s.clarity, s.completeness].iter().all(|v| (0.0..=1.0).contains(v))
            })
            .await
            {
                Ok(s) => s,
                Err(None) => {
                    ctx.publish(&l);
                    return Ok(Outcome::Cancelled);
                }
                Err(Some(v)) => {
                    last_error = Some(verdict_text(&v));
                    reject = "judge_check_error";
                    break;
                }
            };
            if wrong.is_none() && scores.passes(threshold) {
                kept = Some((trace.clone(), answer.unwrap_or_default(), scores, round));
                break;
            }
            reject = if wrong.is_some() && problem.answer.is_some() { "wrong_answer" } else { "below_threshold" };
            let said = match &problem.answer {
                Some(want) => redact(&scores.feedback, want),
                None => scores.feedback.clone(),
            };
            feedback = match wrong {
                Some(w) => format!("{w} {said}").trim().to_string(),
                None => said,
            };
        }

        match kept {
            Some((trace, answer, scores, round)) => {
                let mut rec = json!({
                    "id": format!("{}-{}", ctx.job_id, l.accepted + 1),
                    "problem": problem.problem,
                    "trace": trace,
                    "answer": answer,
                    "scores": { "correctness": scores.correctness, "clarity": scores.clarity, "completeness": scores.completeness },
                    "iterations": round,
                    "model": ctx.snapshot().model,
                });
                if let Some(want) = &problem.answer {
                    rec["reference"] = json!(want);
                }
                ctx.write(&rec).map_err(|e| format!("cannot write the dataset: {e}"))?;
                l.accept();
                l.end_round(1);
            }
            None => {
                if !reject.is_empty() {
                    l.reject(reject);
                }
                l.end_round(0);
            }
        }
        ctx.publish(&l);
    };
    common::finish(ctx, &l, stop, last_error).map(|()| Outcome::Finished)
}

/// The final answer: the last `Answer:` line, else the last `\boxed{…}`.
pub fn final_answer(trace: &str) -> Option<String> {
    let from_line = trace.lines().rev().find_map(|line| {
        let t = line.trim().trim_start_matches(['*', '#', ' ']);
        let lower = t.to_lowercase();
        lower
            .starts_with("answer:")
            .then(|| t.get("answer:".len()..).unwrap_or_default().trim().trim_matches('*').trim().to_string())
    });
    from_line.filter(|a| !a.is_empty()).or_else(|| boxed(trace))
}

/// Content of the last `\boxed{…}`, braces balanced.
fn boxed(s: &str) -> Option<String> {
    let start = s.rfind("\\boxed{")? + "\\boxed{".len();
    let mut depth = 1;
    for (i, c) in s[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..start + i].trim().to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Answers compared after stripping formatting; numbers compared as numbers.
pub fn same_answer(got: &str, want: &str) -> bool {
    let norm = |s: &str| {
        let inner = boxed(s).unwrap_or_else(|| s.to_string());
        inner
            .trim()
            .trim_matches('$')
            .replace("\\text{", "")
            .replace(['{', '}', ' '], "")
            .trim_end_matches('.')
            .to_lowercase()
    };
    let close = |x: f64, y: f64| (x - y).abs() <= 1e-6 * y.abs().max(1.0);
    let (a, b) = (norm(got), norm(want));
    if a == b {
        return true;
    }
    match (number(&a), number(&b)) {
        (Some(x), Some(y)) => close(x, y),
        _ => match (number_with_unit(got), number_with_unit(want)) {
            // The unit is not part of the value, unless both sides name one
            // and they differ ("160 km" is not "160 miles").
            (Some((x, ux)), Some((y, uy))) => close(x, y) && (ux.is_empty() || uy.is_empty() || ux == uy),
            _ => false,
        },
    }
}

/// A number with an optional one-word unit: "160 km" is (160, "km"), "37 °C"
/// is (37, "°c"), a bare "160" has an empty unit. Only a short, plain unit
/// counts, so a second number or word ("7 or 8", "7 or eight") is not one, and
/// a unit that starts with a letter must be set off by a space: "5x" may be an
/// algebraic term, not 5 of something.
fn number_with_unit(s: &str) -> Option<(f64, String)> {
    let inner = boxed(s).unwrap_or_else(|| s.to_string());
    let text = inner.trim().trim_matches('$').replace("\\text{", "").replace(['{', '}'], "");
    let text = text.trim().trim_end_matches('.');
    let digits_end = text
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == ',' || c == '.' || (i == 0 && matches!(c, '-' | '+'))))
        .map_or(text.len(), |(i, _)| i);
    let value = number(text[..digits_end].trim_end_matches([',', '.']))?;
    let rest = &text[digits_end..];
    let unit = rest.trim();
    if unit.is_empty() {
        return Some((value, String::new()));
    }
    let plain = unit.chars().all(|c| c.is_alphabetic() || matches!(c, '%' | '/' | '°' | '²' | '³' | '^' | '·' | '-'));
    let starts_with_letter = unit.chars().next().is_some_and(char::is_alphabetic);
    let spaced = rest.starts_with(char::is_whitespace);
    if !plain || unit.chars().count() > 10 || (starts_with_letter && !spaced) {
        return None;
    }
    Some((value, unit.to_lowercase()))
}

/// A number, with commas allowed only as thousands separators: "1,000" is
/// 1000, but "1,5" (a Vietnamese decimal) and "1,2,3" are not numbers here.
fn number(s: &str) -> Option<f64> {
    let groups: Vec<&str> = s.split('.').next().unwrap_or_default().split(',').collect();
    let thousands = groups.len() > 1
        && (1..=3).contains(&groups[0].len())
        && groups[1..].iter().all(|g| g.len() == 3);
    if s.contains(',') && !thousands {
        return None;
    }
    s.replace(',', "").parse().ok()
}

/// Strike the reference answer out of reviewer feedback before it reaches
/// the generator.
fn redact(feedback: &str, want: &str) -> String {
    let want = want.trim();
    if want.is_empty() {
        return feedback.to_string();
    }
    let lower = feedback.to_lowercase();
    let needle = want.to_lowercase();
    let mut out = String::new();
    let mut last = 0;
    for (i, _) in lower.match_indices(&needle) {
        // Byte offsets from the lowercased copy are only valid on the
        // original when lowercasing kept the length.
        if lower.len() != feedback.len() {
            return feedback.replace(want, "[…]");
        }
        out.push_str(&feedback[last..i]);
        out.push_str("[…]");
        last = i + needle.len();
    }
    out.push_str(&feedback[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datagen::llm::fake::ScriptedLlm;
    use crate::datagen::store::DATA_FILE;

    fn lines(ctx: &JobCtx) -> Vec<serde_json::Value> {
        std::fs::read_to_string(ctx.dir().join(DATA_FILE))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn reads_final_answers_and_compares_them_as_values() {
        assert_eq!(final_answer("step 1\nstep 2\nAnswer: 42").as_deref(), Some("42"));
        assert_eq!(final_answer("so we get\n**Answer:** 7 km").as_deref(), Some("7 km"));
        assert_eq!(final_answer("thus \\boxed{\\frac{1}{2}}").as_deref(), Some("\\frac{1}{2}"));
        assert_eq!(final_answer("no conclusion"), None);
        assert!(!same_answer("\\frac{1}{2}", "\\frac{1}{3}"), "nested braces must not collapse ½ and ⅓");
        assert!(same_answer("1,000", "1000"));
        assert!(same_answer("1,234,567.5", "1234567.5"));
        assert!(!same_answer("1,5", "15"), "a Vietnamese decimal comma is not a thousands separator");
        assert!(!same_answer("1, 2, 3", "123"));
        assert!(same_answer("$0.50$", "0.5"));
        assert!(same_answer("Paris.", "paris"));
        assert!(!same_answer("12", "13"));
    }

    #[test]
    fn a_short_unit_next_to_a_number_does_not_change_the_answer() {
        assert!(same_answer("160 km", "160"));
        assert!(same_answer("160", "160 km"), "the reference may carry the unit instead");
        assert!(same_answer("160 KM", "160 km"));
        assert!(same_answer("60 km/h", "60"));
        assert!(same_answer("$12.50 USD", "12.5"));
        assert!(same_answer("37 °C", "37"));
        assert!(same_answer("50%", "50"));
        assert!(same_answer("\\boxed{160\\text{ km}}", "160"));
        assert!(same_answer("1,000 km.", "1000"));
        assert!(same_answer("-5 °C", "-5"));

        assert!(!same_answer("160 km", "160 miles"), "two different units are different answers");
        assert!(!same_answer("160 km", "161"));
        assert!(!same_answer("12 km", "13 km"));
        assert!(!same_answer("7 or 8", "7"), "a second number is not a unit");
        assert!(!same_answer("7 or eight", "7"), "more than one word is not a unit");
        assert!(!same_answer("5x", "5"), "without a space it may be an algebraic term");
        assert!(!same_answer("1,5 km", "15"), "a decimal comma is still not a thousands separator");
    }

    #[test]
    fn problems_accept_strings_and_objects_with_numeric_answers() {
        let p: Params = serde_json::from_value(json!({
            "problems": ["what is 2+2?", {"question": "3*3?", "final_answer": 9}]
        }))
        .unwrap();
        assert_eq!(p.problems[1], Problem { problem: "3*3?".into(), answer: Some("9".into()) });
        assert!(serde_json::from_value::<Params>(json!({"problems": ["x"], "threshold": 2})).unwrap().validate().is_err());
        assert!(serde_json::from_value::<Params>(json!({"problems": ["x"], "target": 2})).unwrap().validate().is_err());
    }

    #[tokio::test]
    async fn a_wrong_answer_is_improved_without_revealing_the_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p: Params = serde_json::from_value(json!({"problems": [{"problem": "17 * 3 = ?", "answer": "51"}]})).unwrap();
        let llm = Arc::new(ScriptedLlm::new(vec![
            Ok("17 * 3 = 41\nAnswer: 41"),
            Ok(r#"{"correctness":0.2,"clarity":0.9,"completeness":0.8,"feedback":"multiplication slip, it should be 51"}"#),
            Ok("17 * 3 = 17 * 2 + 17 = 34 + 17 = 51\nAnswer: 51"),
            Ok(r#"{"correctness":1,"clarity":0.9,"completeness":0.9,"feedback":""}"#),
        ]));
        run(p, llm.clone(), &mut ctx).await.unwrap();
        let out = lines(&ctx);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["answer"], "51");
        assert_eq!(out[0]["iterations"], 1);
        let prompts = llm.prompts.lock().unwrap();
        let improve = &prompts[2].1;
        assert!(improve.contains("The final answer is wrong") && improve.contains("multiplication slip"));
        assert!(!improve.contains("51"), "the generator never sees the reference answer");
        assert!(improve.contains("[…]"), "the judge's own mention of the answer is struck out");
        assert!(!prompts[1].1.contains("51"), "nor does the judge see the reference");
    }

    #[tokio::test]
    async fn every_dimension_must_pass_and_an_unreadable_judge_rejects() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p: Params = serde_json::from_value(json!({"problems": ["why is the sky blue?", "why do leaves fall?"], "maxIterations": 0})).unwrap();
        let llm = Arc::new(ScriptedLlm::new(vec![
            Ok("Rayleigh scattering...\nAnswer: Rayleigh scattering"),
            Ok(r#"{"correctness":0.95,"clarity":0.95,"completeness":0.5,"feedback":"skips the wavelength argument"}"#),
            Ok("Abscission...\nAnswer: abscission"),
            Ok("looks good"),
            Ok("still not json"),
        ]));
        let err = run(p, llm, &mut ctx).await.unwrap_err();
        assert_eq!(err, "no record passed (1 below_threshold, 1 judge_check_error)");
    }
}

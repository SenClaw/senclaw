//! `expand_seeds`: grow a few example requests into many new ones.
//!
//! Two modes, merged from CAMEL's Self-Instruct and Evol-Instruct:
//! - `new` — show a handful of examples (seeds plus requests this run already
//!   accepted) and ask for new requests of the same kind;
//! - `evolve` — take one example and rewrite it by a named method (add
//!   constraints, make concrete, deepen, reword, move topic).
//!
//! Seeds may carry a label (a category such as a flow name). The run then
//! keeps the categories balanced, and — when there are at least two — sends
//! each batch to a separate classification call that does not see the
//! intended label; a request is kept only when the classifier agrees. That
//! check is what makes the output usable as a labelled eval set, and it fails
//! closed: a reply that cannot be read rejects the batch, never accepts it.
//!
//! Differences from CAMEL on purpose: accepted requests feed back in as
//! examples (CAMEL's `SelfInstructGenerator` never appends them); an empty or
//! unreadable reply is counted and retried, not an `IndexError`; phrase bans
//! match whole words.

use std::collections::BTreeMap;
use std::sync::Arc;

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::common::{self, checked_call, verdict_text};
use super::filters::{tokenize, FilterChain, Known};
use super::generator::{AcceptLoop, Stop};
use super::jobs::{JobCtx, Outcome};
use super::llm::{call, parse_json, CallError, Llm};
use super::verdict::Verdict;

pub const DEFAULT_TARGET: u32 = 50;
pub const MAX_TARGET: u32 = 2000;
pub const MAX_SEEDS: usize = 1000;
const DEFAULT_BATCH: u32 = 5;
const MAX_BATCH: u32 = 10;
const SEED_EXAMPLES: usize = 4;
const GENERATED_EXAMPLES: usize = 2;
/// Rounds in a row a label may come back empty before the others go first.
/// Below the run-wide [`super::generator::MAX_STALL_ROUNDS`], so one hard
/// label cannot end the run before the rest were tried.
const LABEL_STALL_ROUNDS: u32 = 3;

/// Output budgets. Generous, because a reasoning model spends part of the
/// budget thinking before it writes the JSON; a cap that only fits the JSON
/// leaves such a model with nothing readable, and the label check then
/// rejects every batch.
fn generate_max_tokens(n: u32) -> u32 {
    512 + n * 160
}

fn classify_max_tokens(n: usize) -> u32 {
    256 + n as u32 * 48
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    New,
    Evolve,
}

/// One example request. Accepts a bare string, or `{text, label}` (with
/// `instruction` as an alias for `text`, the shape CAMEL's seed files use).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(from = "SeedInput")]
pub struct Seed {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SeedInput {
    Plain(String),
    Full {
        #[serde(alias = "instruction")]
        text: String,
        #[serde(default)]
        label: Option<String>,
    },
}

impl From<SeedInput> for Seed {
    fn from(s: SeedInput) -> Seed {
        let (text, label) = match s {
            SeedInput::Plain(text) => (text, None),
            SeedInput::Full { text, label } => (text, label),
        };
        let label = label.map(|l| l.trim().to_string()).filter(|l| !l.is_empty());
        Seed { text: text.trim().to_string(), label }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Params {
    pub seeds: Vec<Seed>,
    /// Optional description per label, shown to the generator and the
    /// classifier.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default = "default_target")]
    pub target: u32,
    /// Language to write in ("Vietnamese", "en"…); absent = the examples'.
    #[serde(default)]
    pub language: Option<String>,
    /// Default: on when the seeds carry two or more labels.
    #[serde(default)]
    pub check_label: Option<bool>,
    #[serde(default)]
    pub banned_phrases: Vec<String>,
    #[serde(default)]
    pub min_words: Option<usize>,
    #[serde(default)]
    pub max_words: Option<usize>,
    /// ROUGE-L at or above which a candidate counts as a near-duplicate.
    #[serde(default)]
    pub max_similarity: Option<f64>,
    /// Requests asked for per generation call.
    #[serde(default)]
    pub batch: Option<u32>,
    #[serde(default)]
    pub max_llm_calls: Option<u32>,
    /// LLM config id or label; absent = the active model.
    #[serde(default)]
    pub profile: Option<String>,
}

fn default_target() -> u32 {
    DEFAULT_TARGET
}

/// The settings a run actually uses, after defaults and validation.
#[derive(Debug, Clone)]
pub struct Plan {
    pub groups: BTreeMap<Option<String>, Vec<String>>,
    pub check_label: bool,
    pub batch: u32,
    pub max_llm_calls: u32,
    pub filters: FilterChain,
}

impl Params {
    pub fn plan(&self) -> Result<Plan, String> {
        let seeds: Vec<&Seed> = self.seeds.iter().filter(|s| !s.text.is_empty()).collect();
        if seeds.is_empty() {
            return Err("seeds is empty — give at least one example request".into());
        }
        if seeds.len() > MAX_SEEDS {
            return Err(format!("at most {MAX_SEEDS} seeds per run"));
        }
        if !(1..=MAX_TARGET).contains(&self.target) {
            return Err(format!("target must be between 1 and {MAX_TARGET}"));
        }
        let labelled = seeds.iter().filter(|s| s.label.is_some()).count();
        if labelled != 0 && labelled != seeds.len() {
            return Err("label every seed or none of them".into());
        }
        let mut groups: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();
        for s in &seeds {
            groups.entry(s.label.clone()).or_default().push(s.text.clone());
        }
        if let Some(l) = self.labels.keys().find(|l| !groups.contains_key(&Some((*l).clone()))) {
            return Err(format!("label \"{l}\" has no seed to learn from"));
        }
        let distinct = groups.keys().filter(|k| k.is_some()).count();
        let check_label = self.check_label.unwrap_or(distinct >= 2);
        if check_label && distinct < 2 {
            return Err("checking labels needs seeds with at least two different labels".into());
        }
        let batch = self.batch.unwrap_or(DEFAULT_BATCH).clamp(1, MAX_BATCH);
        let max_llm_calls = common::budget(self.max_llm_calls, self.target.div_ceil(batch), 1 + u32::from(check_label));
        let min_words = self.min_words.unwrap_or(3);
        let max_words = self.max_words.unwrap_or(150);
        let max_similarity = self.max_similarity.unwrap_or(0.7);
        if !(0.0..=1.0).contains(&max_similarity) || max_similarity == 0.0 {
            return Err("maxSimilarity must be in (0, 1]".into());
        }
        let filters = FilterChain::new(min_words, max_words, &self.banned_phrases, max_similarity);
        Ok(Plan { groups, check_label, batch, max_llm_calls, filters })
    }
}

#[derive(Deserialize)]
struct Items {
    items: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct Labels {
    labels: Vec<String>,
}

const GENERATE_SYSTEM: &str = "You write realistic messages that people send to a personal AI \
assistant. They become a test set, so they must be varied and natural. Reply with ONLY a JSON \
object, no prose and no code fence: {\"items\": [\"...\", \"...\"]}";

const CLASSIFY_SYSTEM: &str = "You sort messages sent to a personal AI assistant into categories. \
Reply with ONLY a JSON object, no prose and no code fence: {\"labels\": [\"<category>\", ...]} — \
exactly one category per message, in the order given.";

/// Rewrite methods for `evolve`, after Evol-Instruct's in-depth and
/// in-breadth operations.
const EVOLVE_METHODS: &[(&str, &str)] = &[
    ("add_constraints", "Add one or two realistic constraints or requirements (a deadline, a format, a limit)."),
    ("concretize", "Replace general words with specific, concrete details (names, numbers, places)."),
    ("deepen", "Make it need more steps or more reasoning to fulfil."),
    ("reword", "Ask for the same thing with completely different wording and sentence structure."),
    ("shift_topic", "Keep the kind of request but move it to a different topic or domain."),
];

pub async fn run(params: Params, llm: Arc<dyn Llm>, ctx: &mut JobCtx) -> Result<Outcome, String> {
    let plan = params.plan()?;
    let labels_desc = &params.labels;
    let language = params.language.as_deref().map(str::trim).filter(|l| !l.is_empty());
    let all_labels: Vec<String> = plan.groups.keys().flatten().cloned().collect();

    let mut known = Known::default();
    for texts in plan.groups.values() {
        for t in texts {
            known.add(tokenize(t));
        }
    }
    let mut generated: BTreeMap<Option<String>, Vec<String>> = BTreeMap::new();
    let mut per_label: BTreeMap<Option<String>, u32> = plan.groups.keys().map(|k| (k.clone(), 0)).collect();
    let quota = quotas(params.target, plan.groups.keys());
    // Rounds tried and empty rounds in a row, per label.
    let mut tries: BTreeMap<Option<String>, usize> = BTreeMap::new();
    let mut stalls: BTreeMap<Option<String>, u32> = BTreeMap::new();
    let mut rng = StdRng::from_entropy();
    let mut l = AcceptLoop::new(params.target, plan.max_llm_calls);
    let mut last_error: Option<String> = None;
    ctx.set_target(params.target, plan.max_llm_calls);

    let stop = loop {
        if ctx.cancel.is_cancelled() {
            ctx.publish(&l);
            return Ok(Outcome::Cancelled);
        }
        if let Some(stop) = l.stop() {
            break stop;
        }
        let calls_per_round = 1 + u32::from(plan.check_label);
        if !l.can_spend(calls_per_round) {
            break Stop::BudgetSpent;
        }
        // The category furthest behind its share goes next, and asks for little
        // more than its share still needs, so labels end up balanced. A label
        // that keeps coming back empty steps aside for the others.
        let Some(label) = per_label
            .iter()
            .filter(|(k, n)| **n < quota[*k])
            .min_by_key(|(k, n)| (stalls.get(*k).copied().unwrap_or(0) >= LABEL_STALL_ROUNDS, **n))
            .map(|(k, _)| k.clone())
        else {
            break Stop::Done;
        };
        let round = *tries.entry(label.clone()).and_modify(|t| *t += 1).or_insert(0);
        let n = plan.batch.min(quota[&label] - per_label[&label] + 2);
        let seeds = &plan.groups[&label];
        let own = generated.get(&label).map(Vec::as_slice).unwrap_or(&[]);
        let category = category_line(label.as_deref(), labels_desc);
        let (user, method, from) = match params.mode {
            Mode::New => {
                let mut examples: Vec<String> = seeds.choose_multiple(&mut rng, SEED_EXAMPLES).cloned().collect();
                examples.extend(own.choose_multiple(&mut rng, GENERATED_EXAMPLES).cloned());
                (new_prompt(&category, &examples, n, label.is_some(), language), "new".to_string(), examples)
            }
            Mode::Evolve => {
                let pool: Vec<&String> = seeds.iter().chain(own.iter()).collect();
                let base = pool[round % pool.len()].clone();
                let (name, how) = EVOLVE_METHODS[round % EVOLVE_METHODS.len()];
                (evolve_prompt(&category, &base, n, how, label.is_some(), language), format!("evolve:{name}"), vec![base])
            }
        };

        let reply = match call(llm.as_ref(), &ctx.cancel, GENERATE_SYSTEM, &user, generate_max_tokens(n)).await {
            Ok(r) => r,
            Err(CallError::Cancelled) => {
                ctx.publish(&l);
                return Ok(Outcome::Cancelled);
            }
            Err(CallError::Failed(v)) => {
                last_error = Some(verdict_text(&v));
                l.record_call_error();
                l.end_round(0);
                *stalls.entry(label).or_insert(0) += 1;
                ctx.publish(&l);
                continue;
            }
        };
        l.record_call(&reply);
        ctx.set_model(&reply.model);

        let Some(items) = parse_json::<Items>(&reply.text) else {
            l.reject("unparseable_output");
            l.end_round(0);
            *stalls.entry(label).or_insert(0) += 1;
            ctx.publish(&l);
            continue;
        };

        // Cheap filters first. A candidate let through joins `known` at once,
        // so the same batch cannot pass two copies of one request.
        let mut passed: Vec<String> = Vec::new();
        for item in items.items {
            l.attempts += 1;
            let text = item.as_str().map(clean).unwrap_or_default();
            match plan.filters.check(&text, &known) {
                Ok(tokens) => {
                    known.add(tokens);
                    passed.push(text);
                }
                Err(reason) => l.reject(reason),
            }
        }

        let verdicts = if plan.check_label && !passed.is_empty() {
            match classify(llm.as_ref(), ctx, &mut l, &all_labels, labels_desc, &passed).await {
                Ok(found) => passed
                    .iter()
                    .zip(found)
                    .map(|(_, got)| match (&label, got) {
                        (Some(want), Some(got)) if got == *want => Verdict::Success,
                        _ => Verdict::Failure("label_mismatch".into()),
                    })
                    .collect(),
                Err(None) => {
                    ctx.publish(&l);
                    return Ok(Outcome::Cancelled);
                }
                Err(Some(v)) => {
                    if let Verdict::Error(e) = &v {
                        last_error = Some(e.clone());
                    }
                    vec![Verdict::Failure(format!("label_check_{}", v.reason())); passed.len()]
                }
            }
        } else {
            vec![Verdict::Success; passed.len()]
        };

        let mut accepted_now = 0;
        for (text, verdict) in passed.into_iter().zip(verdicts) {
            // Every judged candidate is counted; the caps only discard
            // surplus good ones.
            if verdict != Verdict::Success {
                l.reject(&verdict.reason());
                continue;
            }
            if l.accepted >= l.target || per_label[&label] >= quota[&label] {
                continue;
            }
            let mut rec = json!({
                "id": format!("{}-{}", ctx.job_id, l.accepted + 1),
                "text": text,
                "method": method,
                "from": from,
                "model": reply.model,
            });
            if let Some(lab) = &label {
                rec["label"] = json!(lab);
            }
            ctx.write(&rec).map_err(|e| format!("cannot write the dataset: {e}"))?;
            l.accept();
            accepted_now += 1;
            *per_label.entry(label.clone()).or_insert(0) += 1;
            generated.entry(label.clone()).or_default().push(text);
        }
        l.end_round(accepted_now);
        let s = stalls.entry(label).or_insert(0);
        *s = if accepted_now == 0 { *s + 1 } else { 0 };
        ctx.publish(&l);
    };

    common::finish(ctx, &l, stop, last_error).map(|()| Outcome::Finished)
}

/// One classification call over `texts`, retried once on an unreadable
/// reply. `Ok` holds one label per text (`None` where the reply named no
/// known label); `Err(None)` means the job was cancelled.
async fn classify(
    llm: &dyn Llm,
    ctx: &JobCtx,
    l: &mut AcceptLoop,
    labels: &[String],
    desc: &BTreeMap<String, String>,
    texts: &[String],
) -> Result<Vec<Option<String>>, Option<Verdict>> {
    let mut user = String::from("Categories:\n");
    for lab in labels {
        match desc.get(lab).map(|d| d.trim()).filter(|d| !d.is_empty()) {
            Some(d) => user.push_str(&format!("- {lab}: {d}\n")),
            None => user.push_str(&format!("- {lab}\n")),
        }
    }
    user.push_str("\nMessages:\n");
    for (i, t) in texts.iter().enumerate() {
        user.push_str(&format!("{}. {}\n", i + 1, t.replace('\n', " ")));
    }
    user.push_str(&format!("\nGive exactly {} labels, each one of: {}.", texts.len(), labels.join(", ")));

    let n = texts.len();
    let parsed: Labels =
        checked_call(llm, ctx, l, CLASSIFY_SYSTEM, &user, classify_max_tokens(n), |p: &Labels| p.labels.len() == n).await?;
    Ok(parsed.labels.iter().map(|got| match_label(got, labels)).collect())
}

/// An even share of `target` per label; the first `target % n` labels get
/// one more.
fn quotas<'a>(target: u32, labels: impl Iterator<Item = &'a Option<String>>) -> BTreeMap<Option<String>, u32> {
    let labels: Vec<&Option<String>> = labels.collect();
    let n = labels.len().max(1) as u32;
    labels
        .into_iter()
        .enumerate()
        .map(|(i, l)| (l.clone(), target / n + u32::from((i as u32) < target % n)))
        .collect()
}

/// The label `got` names, compared case-insensitively — Unicode case, since
/// labels may be Vietnamese.
fn match_label(got: &str, labels: &[String]) -> Option<String> {
    let g = got.trim().trim_matches(|c| c == '"' || c == '\'' || c == '`').to_lowercase();
    labels.iter().find(|l| l.to_lowercase() == g).cloned()
}

fn category_line(label: Option<&str>, desc: &BTreeMap<String, String>) -> String {
    match label {
        None => String::new(),
        Some(l) => match desc.get(l).map(|d| d.trim()).filter(|d| !d.is_empty()) {
            Some(d) => format!("Category: {l} — {d}\n"),
            None => format!("Category: {l}\n"),
        },
    }
}

fn language_rule(language: Option<&str>) -> String {
    common::language_rule(language, "Write every message in the same language as the examples.")
}

fn new_prompt(category: &str, examples: &[String], n: u32, labelled: bool, language: Option<&str>) -> String {
    let mut p = String::from(category);
    p.push_str("Examples:\n");
    for e in examples {
        p.push_str(&format!("- {}\n", e.replace('\n', " ")));
    }
    let of_category = if labelled { " that belong to this category" } else { " of the same kind" };
    p.push_str(&format!(
        "\nWrite {n} new messages{of_category}. Each must differ from the examples and from each other \
         in topic and wording — not a paraphrase of any of them. Each is one message a person would \
         actually type. {}",
        language_rule(language)
    ));
    p
}

fn evolve_prompt(category: &str, base: &str, n: u32, how: &str, labelled: bool, language: Option<&str>) -> String {
    let stay = if labelled { " and must still belong to this category" } else { "" };
    format!(
        "{category}Message:\n{base}\n\nRewrite this message into {n} new versions. {how} Each version is one \
         realistic message a person would type{stay}. The versions must differ from each other. {}",
        language_rule(language)
    )
}

fn clean(s: &str) -> String {
    s.trim().trim_matches('"').trim().to_string()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::datagen::jobs::JobCtx;
    use crate::datagen::llm::fake::ScriptedLlm;
    use crate::datagen::store::DATA_FILE;

    fn seeds(v: &[(&str, &str)]) -> Vec<Seed> {
        v.iter().map(|(t, l)| Seed { text: t.to_string(), label: Some(l.to_string()) }).collect()
    }

    fn params(seeds: Vec<Seed>, target: u32) -> Params {
        serde_json::from_value(json!({ "seeds": seeds, "target": target })).unwrap()
    }

    fn lines(ctx: &JobCtx) -> Vec<serde_json::Value> {
        std::fs::read_to_string(ctx.dir().join(DATA_FILE))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_label_that_keeps_failing_steps_aside_for_the_others() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let mut p = params(seeds(&[("open my calendar for friday", "a"), ("make a dataset of quiz questions", "b")]), 2);
        p.max_llm_calls = Some(40);
        // Three rounds for `a` that the classifier files under `b`, then `b`
        // gets its turn and passes.
        let llm = Arc::new(ScriptedLlm::new(vec![
            Ok(r#"{"items":["book a dentist appointment next monday"]}"#),
            Ok(r#"{"labels":["b"]}"#),
            Ok(r#"{"items":["translate this paragraph into japanese please"]}"#),
            Ok(r#"{"labels":["b"]}"#),
            Ok(r#"{"items":["find the cheapest flight from hanoi to tokyo"]}"#),
            Ok(r#"{"labels":["b"]}"#),
            Ok(r#"{"items":["write a limerick about a lazy cat"]}"#),
            Ok(r#"{"labels":["b"]}"#),
        ]));
        run(p, llm, &mut ctx).await.unwrap();
        let out = lines(&ctx);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["label"], "b");
        assert_eq!(ctx.snapshot().rejected.get("label_mismatch"), Some(&3));
    }

    #[tokio::test]
    async fn a_run_where_nothing_passes_fails_with_the_reasons() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p: Params = serde_json::from_value(json!({"seeds": ["write a haiku about the monsoon"], "target": 3})).unwrap();
        let copy = r#"{"items":["Write a haiku about the monsoon."]}"#;
        let llm = Arc::new(ScriptedLlm::new(vec![Ok(copy); 5]));
        let err = run(p, llm, &mut ctx).await.unwrap_err();
        assert_eq!(err, "no record passed (5 duplicate)");
        assert!(ctx.snapshot().note.unwrap().contains("several rounds"));
    }

    #[tokio::test]
    async fn cancelling_during_the_label_check_keeps_the_counts() {
        struct AnswerThenHang(std::sync::atomic::AtomicU32);
        #[async_trait::async_trait]
        impl Llm for AnswerThenHang {
            async fn complete(&self, _: &str, _: &str, _: u32) -> Result<crate::datagen::llm::LlmReply, String> {
                if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    return Ok(crate::datagen::llm::LlmReply {
                        text: r#"{"items":["summarise my unread email from today"]}"#.into(),
                        ..Default::default()
                    });
                }
                std::future::pending().await
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p = params(seeds(&[("book a flight to da nang", "a"), ("generate twenty riddles for kids", "b")]), 2);
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let out = run(p, Arc::new(AnswerThenHang(Default::default())), &mut ctx).await.unwrap();
        assert_eq!(out, Outcome::Cancelled);
        let st = ctx.snapshot();
        assert_eq!((st.llm_calls, st.attempts), (1, 1), "the call made before the cancel is counted");
    }

    #[test]
    fn labels_match_regardless_of_unicode_case() {
        let labels = vec!["Lịch".to_string(), "dữ_liệu".to_string()];
        assert_eq!(match_label(" \"LỊCH\" ", &labels).as_deref(), Some("Lịch"));
        assert_eq!(match_label("DỮ_LIỆU", &labels).as_deref(), Some("dữ_liệu"));
        assert_eq!(match_label("other", &labels), None);
    }

    #[test]
    fn quotas_split_the_target_evenly() {
        let labels = [Some("a".to_string()), Some("b".to_string()), Some("c".to_string())];
        let q = quotas(11, labels.iter());
        assert_eq!(q.values().copied().collect::<Vec<_>>(), vec![4, 4, 3]);
        assert_eq!(quotas(5, [None].iter())[&None], 5);
    }

    #[test]
    fn seeds_accept_strings_objects_and_camel_instruction_files() {
        let p: Params = serde_json::from_value(json!({
            "seeds": ["plain one", {"text": "with label", "label": " a "}, {"instruction": "camel style"}]
        }))
        .unwrap();
        assert_eq!(p.seeds[0], Seed { text: "plain one".into(), label: None });
        assert_eq!(p.seeds[1].label.as_deref(), Some("a"));
        assert_eq!(p.seeds[2].text, "camel style");
    }

    #[test]
    fn plan_validates_labels_and_defaults_the_check() {
        let two = params(seeds(&[("open my calendar for friday", "a"), ("make a dataset of quiz questions", "b")]), 10);
        let plan = two.plan().unwrap();
        assert!(plan.check_label, "two labels turn the check on");
        assert_eq!(plan.max_llm_calls, (10u32.div_ceil(5) * 2 * 3).max(10));

        let mut mixed = two.clone();
        mixed.seeds.push(Seed { text: "no label here".into(), label: None });
        assert!(mixed.plan().unwrap_err().contains("every seed or none"));

        let mut one = params(seeds(&[("open my calendar for friday", "a")]), 10);
        assert!(!one.plan().unwrap().check_label);
        one.check_label = Some(true);
        assert!(one.plan().unwrap_err().contains("two different labels"));

        let mut orphan = two.clone();
        orphan.labels.insert("c".into(), "nobody".into());
        assert!(orphan.plan().unwrap_err().contains("\"c\" has no seed"));

        assert!(params(vec![], 10).plan().is_err());
        assert!(params(seeds(&[("x y z", "a")]), 0).plan().is_err());
    }

    #[tokio::test]
    async fn keeps_only_requests_the_classifier_agrees_with() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p = params(
            seeds(&[
                ("lên lịch họp với team vào thứ sáu", "schedule"),
                ("tạo bộ câu hỏi trắc nghiệm về lịch sử", "dataset"),
            ]),
            2,
        );
        // Labels are balanced, so the first round is `dataset`. The batch has
        // one good request, one near-copy of the seed (filtered), and one the
        // classifier files under the other label.
        let llm = Arc::new(ScriptedLlm::new(vec![
            Ok(r#"{"items":["sinh 50 cặp hỏi đáp về vật lý lớp 10","tạo bộ câu hỏi trắc nghiệm về lịch sử nhé","nhắc tôi gọi mẹ lúc 8 giờ tối"]}"#),
            Ok(r#"{"labels":["dataset","schedule"]}"#),
            Ok(r#"```json
{"items":["đặt lịch khám răng tuần sau"]}
```"#),
            Ok(r#"{"labels":["schedule"]}"#),
        ]));
        run(p, llm.clone(), &mut ctx).await.unwrap();

        let out = lines(&ctx);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["label"], "dataset");
        assert_eq!(out[0]["text"], "sinh 50 cặp hỏi đáp về vật lý lớp 10");
        assert_eq!(out[1]["label"], "schedule");
        let st = ctx.snapshot();
        assert_eq!(st.rejected.get("too_similar"), Some(&1));
        assert_eq!(st.rejected.get("label_mismatch"), Some(&1));
        assert_eq!(st.llm_calls, 4);

        // The classifier never sees which label was intended.
        let prompts = llm.prompts.lock().unwrap();
        assert!(prompts[1].0.contains("sort messages"));
        assert!(!prompts[1].1.contains("Category:"));
    }

    #[tokio::test]
    async fn an_unreadable_classifier_reply_rejects_the_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let mut p = params(seeds(&[("book a flight to da nang", "a"), ("generate twenty riddles for kids", "b")]), 1);
        p.max_llm_calls = Some(3);
        let llm = Arc::new(ScriptedLlm::new(vec![
            Ok(r#"{"items":["summarise my unread email from today"]}"#),
            Ok("I think it is a"),
            Ok("still not json"),
        ]));
        let err = run(p, llm, &mut ctx).await.unwrap_err();
        assert_eq!(err, "the check's reply was not JSON", "a run with nothing accepted fails, naming why");
        assert!(lines(&ctx).is_empty(), "fail closed");
        let st = ctx.snapshot();
        assert_eq!(st.rejected.get("label_check_error"), Some(&1));
        assert_eq!(st.note.as_deref(), Some("stopped early: the LLM call budget was spent"));
    }

    #[tokio::test]
    async fn accepted_requests_become_examples_and_unreadable_output_is_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p: Params = serde_json::from_value(json!({
            "seeds": ["write a haiku about the monsoon"], "target": 2, "batch": 1
        }))
        .unwrap();
        let llm = Arc::new(ScriptedLlm::new(vec![
            Ok("sorry, here are some ideas"),
            Ok(r#"{"items":["draft a polite reply declining the invitation"]}"#),
            Ok(r#"{"items":["list three cheap dinner ideas with eggs"]}"#),
        ]));
        run(p, llm.clone(), &mut ctx).await.unwrap();
        assert_eq!(lines(&ctx).len(), 2);
        assert_eq!(ctx.snapshot().rejected.get("unparseable_output"), Some(&1));
        let prompts = llm.prompts.lock().unwrap();
        assert!(prompts[2].1.contains("draft a polite reply"), "the accepted request is shown as an example");
    }

    #[tokio::test]
    async fn evolve_rewrites_by_method_and_fails_when_the_model_never_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p: Params = serde_json::from_value(json!({
            "seeds": ["plan a trip"], "mode": "evolve", "target": 1, "minWords": 1
        }))
        .unwrap();
        let llm = Arc::new(ScriptedLlm::new(vec![Ok(r#"{"items":["plan a three-day trip to hue under 5 million dong"]}"#)]));
        run(p.clone(), llm.clone(), &mut ctx).await.unwrap();
        assert_eq!(lines(&ctx)[0]["method"], "evolve:add_constraints");
        assert!(llm.prompts.lock().unwrap()[0].1.contains("constraints"));

        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let err = run(p, Arc::new(ScriptedLlm::new(vec![])), &mut ctx).await.unwrap_err();
        assert_eq!(err, "script exhausted");
    }
}

//! `executable_qa`: new computable problems whose answer is what their
//! program prints.
//!
//! After CAMEL's few-shot generator with a Python verifier. The model writes
//! a question and a short Python program; the program runs in SenClaw's own
//! sandbox (`crate::sandbox`) and its stdout becomes the answer — the label
//! comes from execution, not from the model's claim.
//!
//! The one rule that is not CAMEL's: **model-written code never runs
//! unconfined.** CAMEL runs it in a host virtualenv. Here the run must get an
//! enforced boundary (Seatbelt, bubblewrap, AppContainer or a container) with
//! no network; a machine without one refuses the job up front, and a run that
//! somehow comes back unconfined fails it.

use std::sync::Arc;

use async_trait::async_trait;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::common::{self, verdict_text};
use super::filters::{tokenize, FilterChain, Known};
use super::generator::{AcceptLoop, Stop};
use super::jobs::{JobCtx, Outcome};
use super::llm::{call, parse_json, CallError, Llm};

pub const DEFAULT_TARGET: u32 = 50;
pub const MAX_TARGET: u32 = 1000;
pub const MAX_SEEDS: usize = 1000;
const DEFAULT_BATCH: u32 = 3;
const MAX_BATCH: u32 = 5;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 60_000;
const MAX_CODE_CHARS: usize = 8_000;
/// An answer is a value, not a report.
const MAX_ANSWER_CHARS: usize = 200;
const SEED_EXAMPLES: usize = 3;
const GENERATED_EXAMPLES: usize = 2;
/// Isolation labels the sandbox reports for an enforced boundary.
const ENFORCED: &[&str] = &["seatbelt", "bubblewrap", "appcontainer", "container"];

/// An example problem. A bare string, or `{question, answer?, code?}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(from = "SeedInput")]
pub struct QaSeed {
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SeedInput {
    Plain(String),
    Full {
        #[serde(alias = "problem", alias = "text")]
        question: String,
        #[serde(default, alias = "final_answer")]
        answer: Option<serde_json::Value>,
        #[serde(default, alias = "rationale")]
        code: Option<String>,
    },
}

impl From<SeedInput> for QaSeed {
    fn from(s: SeedInput) -> QaSeed {
        match s {
            SeedInput::Plain(q) => QaSeed { question: q.trim().to_string(), answer: None, code: None },
            SeedInput::Full { question, answer, code } => QaSeed {
                question: question.trim().to_string(),
                answer: answer.map(|a| match a {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                }),
                code,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Params {
    #[serde(default)]
    pub seeds: Vec<QaSeed>,
    /// Seeds read from a dataset on disk instead of `seeds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seeds_from: Option<super::from_dataset::DatasetRef>,
    #[serde(default = "default_target")]
    pub target: u32,
    #[serde(default)]
    pub language: Option<String>,
    /// Problems asked for per call.
    #[serde(default)]
    pub batch: Option<u32>,
    /// Per-program run limit.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_similarity: Option<f64>,
    #[serde(default)]
    pub max_llm_calls: Option<u32>,
    #[serde(default)]
    pub profile: Option<String>,
}

fn default_target() -> u32 {
    DEFAULT_TARGET
}

impl Params {
    pub fn validate(&self) -> Result<(), String> {
        let n = self.seeds.iter().filter(|s| !s.question.is_empty()).count();
        if n == 0 {
            return Err("seeds is empty — give at least one example problem".into());
        }
        if n > MAX_SEEDS {
            return Err(format!("at most {MAX_SEEDS} seeds per run"));
        }
        if !(1..=MAX_TARGET).contains(&self.target) {
            return Err(format!("target must be between 1 and {MAX_TARGET}"));
        }
        if let Some(s) = self.max_similarity {
            if !(s > 0.0 && s <= 1.0) {
                return Err("maxSimilarity must be in (0, 1]".into());
            }
        }
        Ok(())
    }
}

/// What a sandboxed run produced.
#[derive(Debug, Clone, Default)]
pub struct CodeRun {
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub timed_out: bool,
    /// The isolation the sandbox applied (`seatbelt`, `container`, `degraded`…).
    pub isolation: String,
}

/// Where model-written programs run. `Err` from either method is an
/// infrastructure failure (no sandbox, Python switched off), which ends the
/// job; a program that fails is an `Ok` run with a non-zero exit.
#[async_trait]
pub trait CodeRunner: Send + Sync {
    /// The backend to run on, or why there is no enforced one.
    async fn preflight(&self) -> Result<String, String>;
    async fn run_python(&self, backend: &str, code: &str, timeout_ms: u64) -> Result<CodeRun, String>;
}

/// The daemon's sandbox engine: a throwaway sandbox per program, no network.
pub struct SandboxRunner;

#[async_trait]
impl CodeRunner for SandboxRunner {
    async fn preflight(&self) -> Result<String, String> {
        // Checked before any model call: a job that cannot run its programs
        // should not spend a generation finding that out.
        let db = crate::sandbox::shared_db().ok_or("the sandbox engine is unavailable")?;
        let policy = crate::sandbox::policy::load(&db);
        if !policy.run_python {
            return Err("Python execution is switched off (Plugins → Sandbox → Run Python)".into());
        }
        let direct = crate::sandbox::caps::direct_caps(false).await;
        // With the master switch off, `direct` would be an ordinary process.
        // Model-written code never runs that way, so only a container is left.
        if policy.enabled && direct.available && direct.kind.is_enforced() {
            return Ok("direct".into());
        }
        // Docker is probed only when the host has no jail of its own; the
        // probe can take seconds when Docker is installed but broken.
        let docker = crate::sandbox::caps::docker_caps(false).await;
        if docker.available {
            return Ok("docker".into());
        }
        let direct_detail = if policy.enabled {
            direct.detail
        } else {
            "the sandbox is switched off (Plugins → Sandbox)".to_string()
        };
        Err(format!(
            "no enforced sandbox on this machine (direct: {direct_detail}, docker: {}) — executable_qa will not run \
             model-written code unconfined",
            docker.detail
        ))
    }

    async fn run_python(&self, backend: &str, code: &str, timeout_ms: u64) -> Result<CodeRun, String> {
        let db = crate::sandbox::shared_db().ok_or("the sandbox engine is unavailable")?;
        let (run, _) = crate::sandbox::runner::run_once(&db, "python", code, Some(backend.to_string()), false, Some(timeout_ms as i64))
            .await
            .map_err(|e| e.to_string())?;
        Ok(CodeRun { exit_code: run.exit_code, stdout: run.stdout, timed_out: run.timed_out, isolation: run.isolation })
    }
}

#[derive(Deserialize)]
struct Items {
    items: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    question: String,
    #[serde(default)]
    code: String,
}

const GENERATE_SYSTEM: &str = "You write new problems whose answer can be computed. For each problem, \
also write a short Python 3 program that computes the answer using only the standard library, with no \
input, files or network, and prints ONLY the final answer on one line. Reply with ONLY a JSON object, no \
prose and no code fence: {\"items\": [{\"question\": \"...\", \"code\": \"...\"}]}";

pub async fn run(params: Params, llm: Arc<dyn Llm>, runner: Arc<dyn CodeRunner>, ctx: &mut JobCtx) -> Result<Outcome, String> {
    params.validate()?;
    let backend = runner.preflight().await?;
    let seeds: Vec<QaSeed> = params.seeds.iter().filter(|s| !s.question.is_empty()).cloned().collect();
    let batch = params.batch.unwrap_or(DEFAULT_BATCH).clamp(1, MAX_BATCH);
    let timeout = params.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1000, MAX_TIMEOUT_MS);
    let max_calls = common::budget(params.max_llm_calls, params.target.div_ceil(batch), 1);
    let filters = FilterChain::new(4, 200, &[], params.max_similarity.unwrap_or(0.7));
    let lang = common::language_rule(params.language.as_deref(), "Write each question in the language of the examples.");
    let mut known = Known::default();
    for s in &seeds {
        known.add(tokenize(&s.question));
    }
    let mut accepted: Vec<(String, String)> = Vec::new();
    let mut rng = StdRng::from_entropy();
    let mut l = AcceptLoop::new(params.target, max_calls);
    let mut last_error = None;
    ctx.set_target(params.target, max_calls);

    let stop = loop {
        if ctx.cancel.is_cancelled() {
            ctx.publish(&l);
            return Ok(Outcome::Cancelled);
        }
        if let Some(stop) = l.stop() {
            break stop;
        }
        if !l.can_spend(1) {
            break Stop::BudgetSpent;
        }
        let n = batch.min(l.remaining() + 1);
        let mut user = String::from("Examples:\n");
        let mut from = Vec::new();
        for s in seeds.choose_multiple(&mut rng, SEED_EXAMPLES) {
            user.push_str(&format!("- Question: {}\n", s.question.replace('\n', " ")));
            if let Some(a) = &s.answer {
                user.push_str(&format!("  Answer: {a}\n"));
            }
            from.push(s.question.clone());
        }
        for (q, a) in accepted.choose_multiple(&mut rng, GENERATED_EXAMPLES) {
            user.push_str(&format!("- Question: {}\n  Answer: {a}\n", q.replace('\n', " ")));
        }
        user.push_str(&format!(
            "\nWrite {n} new problems of the same kind — different numbers and situations, not copies — each with its \
             program. {lang}"
        ));

        let reply = match call(llm.as_ref(), &ctx.cancel, GENERATE_SYSTEM, &user, 512 + common::THINKING_ROOM + n * 400).await {
            Ok(r) => r,
            Err(CallError::Cancelled) => {
                ctx.publish(&l);
                return Ok(Outcome::Cancelled);
            }
            Err(CallError::Failed(v)) => {
                last_error = Some(verdict_text(&v));
                l.record_call_error();
                l.end_round(0);
                ctx.publish(&l);
                continue;
            }
        };
        l.record_call(&reply);
        ctx.set_model(&reply.model);
        let Some(items) = parse_json::<Items>(&reply.text) else {
            l.reject(common::unreadable(&reply));
            l.end_round(0);
            ctx.publish(&l);
            continue;
        };

        let mut accepted_now = 0;
        for item in items.items {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if l.accepted >= l.target {
                break;
            }
            l.attempts += 1;
            let question = item.question.trim().to_string();
            let code = strip_fence(&item.code);
            if code.trim().is_empty() || code.len() > MAX_CODE_CHARS {
                l.reject("bad_code");
                continue;
            }
            let tokens = match filters.check(&question, &known) {
                Ok(t) => t,
                Err(reason) => {
                    l.reject(reason);
                    continue;
                }
            };
            let run = runner.run_python(&backend, &code, timeout).await?;
            if !ENFORCED.contains(&run.isolation.as_str()) {
                return Err(format!(
                    "the sandbox ran a program with isolation \"{}\" — stopping rather than run model code unconfined",
                    run.isolation
                ));
            }
            let answer = run.stdout.trim().to_string();
            let reject = if run.timed_out {
                Some("timeout")
            } else if run.exit_code != Some(0) {
                Some("code_failed")
            } else if answer.is_empty() {
                Some("empty_output")
            } else if answer.chars().count() > MAX_ANSWER_CHARS {
                Some("long_output")
            } else {
                None
            };
            if let Some(r) = reject {
                l.reject(r);
                continue;
            }
            known.add(tokens);
            let rec = json!({
                "id": format!("{}-{}", ctx.job_id, l.accepted + 1),
                "question": question,
                "answer": answer,
                "code": code,
                "isolation": run.isolation,
                "from": from,
                "model": reply.model,
            });
            ctx.write(&rec).map_err(|e| format!("cannot write the dataset: {e}"))?;
            accepted.push((question, answer));
            l.accept();
            accepted_now += 1;
        }
        l.end_round(accepted_now);
        ctx.publish(&l);
    };
    common::finish(ctx, &l, stop, last_error).map(|()| Outcome::Finished)
}

/// Models often wrap the program in a Markdown fence despite being asked not to.
fn strip_fence(code: &str) -> String {
    let t = code.trim();
    let Some(rest) = t.strip_prefix("```") else { return t.to_string() };
    let body = rest.split_once('\n').map(|(_, b)| b).unwrap_or("");
    body.trim_end().trim_end_matches("```").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datagen::llm::fake::ScriptedLlm;
    use crate::datagen::store::DATA_FILE;
    use std::sync::Mutex;

    /// Runs nothing: answers by program text.
    struct FakeRunner {
        backend: Result<String, String>,
        isolation: &'static str,
        ran: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CodeRunner for FakeRunner {
        async fn preflight(&self) -> Result<String, String> {
            self.backend.clone()
        }
        async fn run_python(&self, _backend: &str, code: &str, _t: u64) -> Result<CodeRun, String> {
            self.ran.lock().unwrap().push(code.to_string());
            let (exit, out, timed_out) = if code.contains("raise") {
                (Some(1), String::new(), false)
            } else if code.contains("while True") {
                (None, String::new(), true)
            } else {
                (Some(0), code.rsplit("print(").next().unwrap().trim_end_matches(')').trim_matches('"').to_string() + "\n", false)
            };
            Ok(CodeRun { exit_code: exit, stdout: out, timed_out, isolation: self.isolation.into() })
        }
    }

    fn runner(isolation: &'static str) -> Arc<FakeRunner> {
        Arc::new(FakeRunner { backend: Ok("direct".into()), isolation, ran: Mutex::default() })
    }

    #[tokio::test]
    async fn stdout_is_the_label_and_failed_programs_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p: Params = serde_json::from_value(json!({
            "seeds": [{"question": "A shop sells 3 pens for 12,000 VND. How much do 7 pens cost?", "answer": 28000}],
            "target": 1
        }))
        .unwrap();
        let llm = Arc::new(ScriptedLlm::new(vec![Ok(r#"{"items":[
            {"question":"A train travels 120 km in 1.5 hours. What is its average speed in km/h?","code":"```python\nprint(\"80\")\n```"},
            {"question":"How many seconds are there in a leap year of 366 days?","code":"raise SystemExit(1)"}
        ]}"#)]));
        let r = runner("seatbelt");
        run(p, llm, r.clone(), &mut ctx).await.unwrap();
        let out: Vec<serde_json::Value> = std::fs::read_to_string(ctx.dir().join(DATA_FILE))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["answer"], "80");
        assert_eq!(out[0]["code"], "print(\"80\")", "the fence was stripped before running");
        assert_eq!(r.ran.lock().unwrap().len(), 1, "the target was reached; the second program never ran");
    }

    #[tokio::test]
    async fn refuses_to_run_without_an_enforced_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p: Params = serde_json::from_value(json!({"seeds": ["What is 2 to the power of 10?"]})).unwrap();
        let none = Arc::new(FakeRunner { backend: Err("no enforced sandbox".into()), isolation: "", ran: Mutex::default() });
        let llm = Arc::new(ScriptedLlm::new(vec![]));
        assert_eq!(run(p.clone(), llm.clone(), none, &mut ctx).await.unwrap_err(), "no enforced sandbox");
        assert!(llm.prompts.lock().unwrap().is_empty(), "no model call before the sandbox is known");

        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let llm = Arc::new(ScriptedLlm::new(vec![Ok(
            r#"{"items":[{"question":"What is 3 to the power of 5, as an integer?","code":"print(243)"}]}"#,
        )]));
        let err = run(p, llm, runner("degraded"), &mut ctx).await.unwrap_err();
        assert!(err.contains("isolation \"degraded\""), "{err}");
    }

    #[test]
    fn strips_markdown_fences() {
        assert_eq!(strip_fence("```python\nprint(1)\n```"), "print(1)");
        assert_eq!(strip_fence("print(2)"), "print(2)");
    }
}

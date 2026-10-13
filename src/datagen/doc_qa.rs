//! `doc_qa`: multi-hop question–answer pairs grounded in documents.
//!
//! After CAMEL's Source2Synth, with the checks it lacks. Documents (inline
//! text, or `.md` / `.txt` files and folders) are cut into passages; for each
//! passage one call writes question–answer pairs that need two or more facts
//! of it, each fact quoted. A pair is kept only when:
//! - each quoted fact is really in the passage, in order: 80% of its words
//!   must match, in sequence, inside a stretch of the passage barely longer
//!   than the quote (checked in code, not by a model — `ungrounded`
//!   otherwise; scattered words that happen to occur do not count),
//! - with `multiHop` on, its facts come from at least two different
//!   sentences (`single_hop`) — two halves of one sentence are one fact,
//! - it is not a near-copy of a question already kept (`too_similar`),
//! - and, with `verify` on (default), a separate call that sees the passage
//!   and the pair confirms the passage alone supports the answer. That check
//!   fails closed: an unreadable reply rejects the batch.
//!
//! Each record carries its passage as `context`, so the set doubles as a
//! retrieval eval (question → the passage that answers it).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::common::{self, checked_call, verdict_text};
use super::filters::{tokenize, FilterChain, Known};
use super::generator::{AcceptLoop, Stop};
use super::jobs::{JobCtx, Outcome};
use super::llm::{call, parse_json, CallError, Llm};

pub const DEFAULT_TARGET: u32 = 50;
pub const MAX_TARGET: u32 = 2000;
const DEFAULT_PER_PASSAGE: u32 = 3;
const MAX_PER_PASSAGE: u32 = 6;
/// Passage size in the memory chunker's token estimate.
const PASSAGE_TOKENS: usize = 350;
/// A passage needs this many words and two sentences to hold a multi-hop question.
const MIN_PASSAGE_WORDS: usize = 30;
const MAX_FILES: usize = 500;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 20 * 1024 * 1024;
const MAX_INLINE_TEXTS: usize = 1000;
/// A quoted fact counts as present when this share of its words match, in
/// order, within a passage window [`WINDOW_SLACK`] words longer than the
/// quote — models trim and re-punctuate quotes, but must not stitch one.
const FACT_COVERAGE: f64 = 0.8;
const WINDOW_SLACK: usize = 2;
/// Folders a document search never enters: dependencies and build output,
/// not documents.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "Library", "dist", "build", "vendor"];
const DOC_EXTENSIONS: &[&str] = &["md", "markdown", "txt"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Params {
    /// Documents given inline.
    #[serde(default)]
    pub texts: Vec<String>,
    /// Absolute paths of `.md` / `.markdown` / `.txt` files, or folders searched for them.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default = "default_target")]
    pub target: u32,
    #[serde(default)]
    pub language: Option<String>,
    /// Pairs asked for per passage.
    #[serde(default)]
    pub per_passage: Option<u32>,
    /// Require two or more supporting facts per pair (default on).
    #[serde(default)]
    pub multi_hop: Option<bool>,
    /// Confirm each pair with a separate grounding check (default on).
    #[serde(default)]
    pub verify: Option<bool>,
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
        if self.texts.iter().all(|t| t.trim().is_empty()) && self.paths.iter().all(|p| p.trim().is_empty()) {
            return Err("give documents: texts, or paths of .md/.txt files or folders".into());
        }
        if self.texts.len() > MAX_INLINE_TEXTS {
            return Err(format!("at most {MAX_INLINE_TEXTS} inline texts"));
        }
        if let Some(p) = self.paths.iter().find(|p| !p.trim().is_empty() && !Path::new(p.trim()).is_absolute()) {
            return Err(format!("path \"{p}\" is not absolute"));
        }
        if !(1..=MAX_TARGET).contains(&self.target) {
            return Err(format!("target must be between 1 and {MAX_TARGET}"));
        }
        Ok(())
    }

    fn per_passage(&self) -> u32 {
        self.per_passage.unwrap_or(DEFAULT_PER_PASSAGE).clamp(1, MAX_PER_PASSAGE)
    }

    fn verify(&self) -> bool {
        self.verify.unwrap_or(true)
    }
}

/// One passage and where it came from.
#[derive(Debug, Clone)]
pub struct Passage {
    pub doc: String,
    pub index: usize,
    pub text: String,
}

#[derive(Deserialize)]
struct Items {
    items: Vec<Item>,
}

#[derive(Deserialize, Clone)]
struct Item {
    #[serde(default)]
    question: String,
    #[serde(default)]
    answer: String,
    #[serde(default)]
    reasoning_steps: Vec<String>,
    #[serde(default)]
    supporting_facts: Vec<String>,
}

#[derive(Deserialize)]
struct Supported {
    supported: Vec<bool>,
}

const GENERATE_SYSTEM: &str = "You write question–answer pairs for testing reading comprehension. \
Every answer must follow from the given passage alone. Reply with ONLY a JSON object, no prose and no \
code fence: {\"items\": [{\"question\": \"...\", \"answer\": \"...\", \"reasoning_steps\": [\"...\"], \
\"supporting_facts\": [\"exact quote\", \"exact quote\"]}]}";

const VERIFY_SYSTEM: &str = "You check question–answer pairs against a passage. For each pair, say true \
only when the passage alone fully supports the answer to the question, false otherwise. Reply with ONLY a \
JSON object, no prose and no code fence: {\"supported\": [true, false, ...]} — one value per pair, in order.";

pub async fn run(params: Params, llm: Arc<dyn Llm>, ctx: &mut JobCtx) -> Result<Outcome, String> {
    params.validate()?;
    let p = params.clone();
    let passages = tokio::task::spawn_blocking(move || load_passages(&p))
        .await
        .map_err(|e| e.to_string())??;
    if passages.is_empty() {
        return Err(format!(
            "no passage is long enough to ask a multi-hop question about (each needs {MIN_PASSAGE_WORDS}+ words and two sentences)"
        ));
    }

    let per_passage = params.per_passage();
    let verify = params.verify();
    let multi_hop = params.multi_hop.unwrap_or(true);
    let max_calls = common::budget(params.max_llm_calls, params.target.div_ceil(per_passage), 1 + u32::from(verify));
    let filters = FilterChain::new(4, 80, &[], 0.7);
    let language = params.language.as_deref();
    let mut known = Known::default();
    let mut l = AcceptLoop::new(params.target, max_calls);
    let mut last_error = None;
    ctx.set_target(params.target, max_calls);

    let mut queue = passages.into_iter();
    let stop = loop {
        if ctx.cancel.is_cancelled() {
            ctx.publish(&l);
            return Ok(Outcome::Cancelled);
        }
        if let Some(stop) = l.stop() {
            break stop;
        }
        if !l.can_spend(1 + u32::from(verify)) {
            break Stop::BudgetSpent;
        }
        let Some(passage) = queue.next() else {
            ctx.set_note("ran out of passages before reaching the target");
            break Stop::Done;
        };
        let n = per_passage.min(l.remaining() + 1);
        let user = format!(
            "Passage:\n\"\"\"\n{}\n\"\"\"\n\nWrite {n} question–answer pairs about this passage.{} Quote each supporting fact \
             exactly as it appears in the passage. Keep answers short. {}",
            passage.text,
            if multi_hop {
                " Each question must need at least two different facts from the passage to answer — not a single sentence lookup."
            } else {
                ""
            },
            common::language_rule(language, "Write in the language of the passage.")
        );
        let reply = match call(llm.as_ref(), &ctx.cancel, GENERATE_SYSTEM, &user, 512 + n * 300).await {
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
            l.reject("unparseable_output");
            l.end_round(0);
            ctx.publish(&l);
            continue;
        };

        let sentences = sentence_tokens(&passage.text);
        let mut passed: Vec<Item> = Vec::new();
        for item in items.items {
            l.attempts += 1;
            let item = Item {
                question: item.question.trim().to_string(),
                answer: item.answer.trim().to_string(),
                ..item
            };
            if item.answer.is_empty() {
                l.reject("no_answer");
                continue;
            }
            let facts: Vec<&String> = item.supporting_facts.iter().filter(|f| !f.trim().is_empty()).collect();
            let located: Vec<Option<usize>> = facts.iter().map(|f| locate(f, &sentences)).collect();
            if facts.is_empty() || located.iter().any(Option::is_none) {
                l.reject("ungrounded");
                continue;
            }
            let mut sentence_ids: Vec<usize> = located.into_iter().flatten().collect();
            sentence_ids.sort_unstable();
            sentence_ids.dedup();
            if multi_hop && sentence_ids.len() < 2 {
                l.reject("single_hop");
                continue;
            }
            match filters.check(&item.question, &known) {
                Ok(tokens) => {
                    known.add(tokens);
                    passed.push(item);
                }
                Err(reason) => l.reject(reason),
            }
        }

        let supported: Vec<bool> = if verify && !passed.is_empty() {
            let mut user = format!("Passage:\n\"\"\"\n{}\n\"\"\"\n\nPairs:\n", passage.text);
            for (i, it) in passed.iter().enumerate() {
                user.push_str(&format!("{}. Q: {}\n   A: {}\n", i + 1, it.question, it.answer));
            }
            user.push_str(&format!("\nGive exactly {} values.", passed.len()));
            let n = passed.len();
            match checked_call(llm.as_ref(), ctx, &mut l, VERIFY_SYSTEM, &user, 256 + n as u32 * 16, |s: &Supported| {
                s.supported.len() == n
            })
            .await
            {
                Ok(s) => s.supported,
                Err(None) => {
                    ctx.publish(&l);
                    return Ok(Outcome::Cancelled);
                }
                Err(Some(v)) => {
                    last_error = Some(verdict_text(&v));
                    for _ in 0..n {
                        l.reject("verify_check_error");
                    }
                    l.end_round(0);
                    ctx.publish(&l);
                    continue;
                }
            }
        } else {
            vec![true; passed.len()]
        };

        let mut accepted_now = 0;
        for (it, ok) in passed.into_iter().zip(supported) {
            if !ok {
                l.reject("unsupported");
                continue;
            }
            if l.accepted >= l.target {
                continue;
            }
            let rec = json!({
                "id": format!("{}-{}", ctx.job_id, l.accepted + 1),
                "question": it.question,
                "answer": it.answer,
                "reasoning_steps": it.reasoning_steps,
                "supporting_facts": it.supporting_facts,
                "context": passage.text,
                "source": { "doc": passage.doc, "passage": passage.index },
                "model": reply.model,
            });
            ctx.write(&rec).map_err(|e| format!("cannot write the dataset: {e}"))?;
            l.accept();
            accepted_now += 1;
        }
        l.end_round(accepted_now);
        ctx.publish(&l);
    };
    common::finish(ctx, &l, stop, last_error).map(|()| Outcome::Finished)
}

/// Sentences: a `.`, `!`, `?` or `。` followed by whitespace or the end, or
/// a line break. A dot inside a number ("1.500 mét") is not a boundary.
fn sentences(passage: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = passage.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next_blank = chars.peek().is_none_or(|(_, n)| n.is_whitespace());
        if c == '\n' || (matches!(c, '.' | '!' | '?' | '。') && next_blank) {
            let end = i + c.len_utf8();
            if !passage[start..end].trim().is_empty() {
                out.push(&passage[start..end]);
            }
            start = end;
        }
    }
    if !passage[start..].trim().is_empty() {
        out.push(&passage[start..]);
    }
    out
}

/// The passage as sentences of tokens.
fn sentence_tokens(passage: &str) -> Vec<Vec<String>> {
    sentences(passage).into_iter().map(tokenize).filter(|t| !t.is_empty()).collect()
}

/// The sentence a quoted fact comes from, or `None` when it is not in the
/// passage. Slides a window a little longer than the quote over the whole
/// passage and keeps the best in-order match; the fact is present when that
/// match covers [`FACT_COVERAGE`] of its words, and it belongs to the
/// sentence the window starts in.
fn locate(fact: &str, sentences: &[Vec<String>]) -> Option<usize> {
    let words = tokenize(fact);
    if words.is_empty() {
        return None;
    }
    let flat: Vec<(usize, &String)> =
        sentences.iter().enumerate().flat_map(|(i, s)| s.iter().map(move |w| (i, w))).collect();
    let tokens: Vec<String> = flat.iter().map(|(_, w)| (*w).clone()).collect();
    let width = words.len() + WINDOW_SLACK;
    let need = (words.len() as f64 * FACT_COVERAGE).ceil() as usize;
    let mut best: Option<(usize, usize)> = None;
    for start in 0..tokens.len() {
        // A window that does not begin on one of the quote's words can only
        // match less than the one that does.
        if !words.contains(&tokens[start]) {
            continue;
        }
        let end = (start + width).min(tokens.len());
        let m = super::filters::lcs_len(&words, &tokens[start..end]);
        if best.is_none_or(|(b, _)| m > b) {
            best = Some((m, flat[start].0));
        }
    }
    best.filter(|(m, _)| *m >= need).map(|(_, sentence)| sentence)
}

/// Every usable passage, documents interleaved so a short target still
/// draws from all of them.
fn load_passages(p: &Params) -> Result<Vec<Passage>, String> {
    let mut docs: Vec<(String, String)> = p
        .texts
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.trim().is_empty())
        .map(|(i, t)| (format!("text-{}", i + 1), t.clone()))
        .collect();
    let mut files: Vec<PathBuf> = Vec::new();
    let private = crate::util::paths::senclaw_home().canonicalize().ok();
    for raw in p.paths.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let path = PathBuf::from(raw);
        let real = path.canonicalize().map_err(|_| format!("{} does not exist", path.display()))?;
        // The daemon's own folder holds API keys, tokens and every chat; it is
        // never a document source, however it is reached.
        if private.as_ref().is_some_and(|home| real.starts_with(home)) {
            return Err(format!("{} is inside SenClaw's own data folder and cannot be read as a document", path.display()));
        }
        if real.is_dir() {
            collect_files(&real, 0, &mut files);
        } else if is_doc(&real) {
            files.push(real);
        } else {
            return Err(format!("{} is not a .md or .txt file", path.display()));
        }
    }
    files.truncate(MAX_FILES);
    let mut total = 0u64;
    for f in files {
        let len = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
        if len > MAX_FILE_BYTES {
            continue;
        }
        total += len;
        if total > MAX_TOTAL_BYTES {
            break;
        }
        if let Ok(text) = std::fs::read_to_string(&f) {
            docs.push((f.to_string_lossy().into_owned(), text));
        }
    }

    let opts = crate::memory::chunker::ChunkerOptions { chunk_size: PASSAGE_TOKENS, chunk_overlap: 0 };
    let per_doc: Vec<Vec<Passage>> = docs
        .into_iter()
        .map(|(doc, text)| {
            crate::memory::chunker::chunk_text(&text, opts)
                .into_iter()
                .map(|c| c.text.trim().to_string())
                .filter(|t| usable(t))
                .enumerate()
                .map(|(index, text)| Passage { doc: doc.clone(), index, text })
                .collect()
        })
        .collect();
    let longest = per_doc.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = Vec::new();
    for i in 0..longest {
        for d in &per_doc {
            if let Some(p) = d.get(i) {
                out.push(p.clone());
            }
        }
    }
    Ok(out)
}

fn usable(text: &str) -> bool {
    text.split_whitespace().count() >= MIN_PASSAGE_WORDS && sentences(text).len() >= 2
}

fn is_doc(p: &Path) -> bool {
    p.is_file() && p.extension().and_then(|e| e.to_str()).is_some_and(|e| DOC_EXTENSIONS.contains(&e))
}

fn collect_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 5 || out.len() >= MAX_FILES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.starts_with('.') || SKIP_DIRS.contains(&name) {
            continue;
        }
        // Symlinks are not followed: one can point anywhere, including back
        // into SenClaw's own folder.
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            collect_files(&p, depth + 1, out);
        } else if is_doc(&p) {
            out.push(p);
        }
        if out.len() >= MAX_FILES {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datagen::llm::fake::ScriptedLlm;
    use crate::datagen::store::DATA_FILE;

    const DOC: &str = "Đà Lạt nằm trên cao nguyên Lâm Viên ở độ cao khoảng 1.500 mét. Nhiệt độ trung bình năm của thành phố \
        là 18 độ C. Người Pháp xây dựng Đà Lạt làm nơi nghỉ dưỡng vào đầu thế kỷ 20. Hồ Xuân Hương nằm ở trung tâm \
        thành phố và được tạo ra năm 1919 bằng cách đắp đập. Ga Đà Lạt được khánh thành năm 1938.";

    fn params(v: serde_json::Value) -> Params {
        serde_json::from_value(v).unwrap()
    }

    fn lines(ctx: &JobCtx) -> Vec<serde_json::Value> {
        std::fs::read_to_string(ctx.dir().join(DATA_FILE))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn grounding_needs_the_quote_in_order_and_finds_its_sentence() {
        let s = sentence_tokens(DOC);
        assert_eq!(sentences("Cao 1.500 mét. Lạnh!\nHết").len(), 3, "a dot inside a number is not a boundary");
        assert_eq!(locate("Hồ Xuân Hương nằm ở trung tâm thành phố", &s), Some(3));
        assert_eq!(locate("\"nhiệt độ trung bình năm của thành phố là 18 độ C\"", &s), Some(1));
        assert_eq!(locate("Đà Lạt có sân bay quốc tế Liên Khương từ năm 1933", &s), None);
        // Real words, stitched: the lake was made in 1919, the station opened in 1938.
        assert_eq!(locate("Ga Đà Lạt được tạo ra năm 1919 bằng cách đắp đập", &s), None);
    }

    #[test]
    fn document_paths_refuse_other_files_and_senclaw_s_own_folder() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.json"), "{}").unwrap();
        let p = params(json!({"paths": [tmp.path().join("config.json")]}));
        assert!(load_passages(&p).unwrap_err().contains("not a .md or .txt"));
        let home = crate::util::paths::senclaw_home();
        if home.exists() {
            let p = params(json!({"paths": [home]}));
            assert!(load_passages(&p).unwrap_err().contains("SenClaw's own data folder"));
        }
        #[cfg(unix)]
        {
            std::fs::create_dir(tmp.path().join("docs")).unwrap();
            std::fs::write(tmp.path().join("real.md"), DOC).unwrap();
            std::os::unix::fs::symlink(tmp.path().join("real.md"), tmp.path().join("docs/link.md")).unwrap();
            let p = params(json!({"paths": [tmp.path().join("docs")]}));
            assert!(load_passages(&p).unwrap().is_empty(), "symlinks are not followed");
        }
    }

    #[test]
    fn validation_and_passage_selection() {
        assert!(params(json!({})).validate().is_err());
        assert!(params(json!({"paths": ["relative/notes.md"]})).validate().unwrap_err().contains("not absolute"));
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.md"), DOC).unwrap();
        std::fs::write(tmp.path().join("short.txt"), "Too short.").unwrap();
        std::fs::write(tmp.path().join("skip.rs"), DOC).unwrap();
        std::fs::create_dir(tmp.path().join(".hidden")).unwrap();
        std::fs::write(tmp.path().join(".hidden/b.md"), DOC).unwrap();
        let p = params(json!({"paths": [tmp.path()], "texts": [DOC]}));
        let passages = load_passages(&p).unwrap();
        let docs: Vec<&str> = passages.iter().map(|p| p.doc.as_str()).collect();
        assert_eq!(docs.len(), 2, "{docs:?}");
        assert!(docs.contains(&"text-1") && docs.iter().any(|d| d.ends_with("a.md")));
    }

    #[tokio::test]
    async fn keeps_grounded_multi_hop_pairs_the_check_confirms() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p = params(json!({"texts": [DOC], "target": 1}));
        let llm = Arc::new(ScriptedLlm::new(vec![
            Ok(r#"{"items":[
              {"question":"Hồ Xuân Hương có trước ga Đà Lạt bao nhiêu năm?","answer":"19 năm",
               "reasoning_steps":["Hồ tạo năm 1919","Ga khánh thành năm 1938"],
               "supporting_facts":["được tạo ra năm 1919 bằng cách đắp đập","Ga Đà Lạt được khánh thành năm 1938"]},
              {"question":"Đà Lạt có sân bay từ năm nào vậy?","answer":"1933",
               "supporting_facts":["sân bay Liên Khương mở năm 1933","Đà Lạt nằm trên cao nguyên"]},
              {"question":"Đà Lạt nằm ở độ cao khoảng bao nhiêu?","answer":"1.500 mét",
               "supporting_facts":["ở độ cao khoảng 1.500 mét"]}
            ]}"#),
            Ok(r#"{"supported":[true]}"#),
        ]));
        run(p, llm.clone(), &mut ctx).await.unwrap();
        let out = lines(&ctx);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["answer"], "19 năm");
        assert!(out[0]["context"].as_str().unwrap().contains("Lâm Viên"));
        let st = ctx.snapshot();
        assert_eq!(st.rejected.get("ungrounded"), Some(&1));
        assert_eq!(st.rejected.get("single_hop"), Some(&1));
        // The check saw the passage and the pair.
        let prompts = llm.prompts.lock().unwrap();
        assert!(prompts[1].1.contains("Lâm Viên") && prompts[1].1.contains("19 năm"));
    }

    #[tokio::test]
    async fn an_unreadable_check_rejects_the_batch_and_the_run_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = JobCtx::for_test(tmp.path());
        let p = params(json!({"texts": [DOC], "target": 1, "maxLlmCalls": 3}));
        let llm = Arc::new(ScriptedLlm::new(vec![
            Ok(r#"{"items":[{"question":"Hồ Xuân Hương có trước ga Đà Lạt bao nhiêu năm?","answer":"19 năm",
               "supporting_facts":["được tạo ra năm 1919","Ga Đà Lạt được khánh thành năm 1938"]}]}"#),
            Ok("yes"),
            Ok("yes, supported"),
        ]));
        let err = run(p, llm, &mut ctx).await.unwrap_err();
        assert_eq!(err, "the check's reply was not JSON");
        assert_eq!(ctx.snapshot().rejected.get("verify_check_error"), Some(&1));
    }
}

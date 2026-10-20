//! What a paired SLink peer may ask this daemon about what it knows.
//!
//! Nothing here copies knowledge anywhere: a peer asks a question
//! (`knowledge/recall`) and gets the few best matches back, over the same sealed
//! channel as tasks. What it may see is the operator's decision, per peer
//! ([`KnowledgePolicy`], off until switched on), and the rules for what can
//! never leave live in one place, [`classify_note`] and [`Scope::AgentWide`], so
//! the preview a person reads before enabling sharing and the answer a peer
//! gets cannot disagree.
//!
//! Search is by words (the fact store's keyword lookup, a token overlap over
//! curated notes): no model call and no embedding, the way the agent's own
//! memory tools work, so a peer's question never costs this machine a model.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::a2a;
use crate::db::Db;
use crate::memory::curated::{is_daily_log_name, memory_session_rank, read_meta, MemoryMeta};
use crate::memory::engine::facts::defense::{scrub, Mode, Verdict};
use crate::memory::engine::facts::store::{FactStore, Scope, StoredFact};
use crate::memory::engine::facts::tools::lookup;
use crate::memory::tokenizer::tokenize_optimized;

/// Version of the knowledge methods, advertised in the Agent Card.
pub const KNOWLEDGE_VERSION: u32 = 1;
pub const METHOD_RECALL: &str = "knowledge/recall";
/// Most hits one answer carries.
pub const MAX_HITS: usize = 10;
const DEFAULT_HITS: usize = 5;
/// A question is cut here; nobody needs more to find a note.
const MAX_QUERY_CHARS: usize = 300;
/// Each hit's text is cut here, so an answer stays small whatever the notes hold.
const MAX_TEXT_CHARS: usize = 600;
/// A note larger than this is skipped, not read.
const MAX_NOTE_BYTES: u64 = 256 * 1024;
/// Notes are scored on their first part only.
const MAX_SCORED_CHARS: usize = 20_000;
/// Profiles one policy may name.
const MAX_PROFILES: usize = 32;
/// Calls one peer may make in a minute.
const CALLS_PER_MINUTE: usize = 30;
const WINDOW: Duration = Duration::from_secs(60);

/// Note types a person may share. `user` describes the owner and never leaves.
pub const SHAREABLE_TYPES: [&str; 3] = ["project", "reference", "feedback"];
/// What `noteTypes: []` means when notes are on.
pub const DEFAULT_NOTE_TYPES: [&str; 2] = ["project", "reference"];

/// What one peer may read of this daemon's knowledge. Everything is off until a
/// person turns it on, per peer and per profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KnowledgePolicy {
    /// Facts that belong to the whole agent (not said in one chat).
    pub facts: bool,
    /// Curated notes of the shareable types, written for the whole agent.
    pub notes: bool,
    /// Profile folders (the agent's name on disk) the two switches above apply to.
    pub profiles: Vec<String>,
    /// Which note types: a subset of [`SHAREABLE_TYPES`]; empty means [`DEFAULT_NOTE_TYPES`].
    pub note_types: Vec<String>,
}

impl KnowledgePolicy {
    /// Does it let a peer ask anything at all?
    pub fn open(&self) -> bool {
        (self.facts || self.notes) && !self.profiles.is_empty()
    }

    pub fn effective_note_types(&self) -> Vec<String> {
        if self.note_types.is_empty() {
            DEFAULT_NOTE_TYPES.iter().map(|t| t.to_string()).collect()
        } else {
            self.note_types.clone()
        }
    }

    /// The policy a person asked for, or why it cannot be. `exists` says whether
    /// a profile folder is there.
    pub fn validated(mut self, exists: impl Fn(&str) -> bool) -> Result<Self, String> {
        let mut seen = HashSet::new();
        let mut profiles = Vec::new();
        for p in &self.profiles {
            let p = p.trim();
            if !safe_profile(p) {
                return Err(format!("profile `{p}` is not a valid profile folder name"));
            }
            if !exists(p) {
                return Err(format!("there is no profile `{p}` on this machine"));
            }
            if seen.insert(p.to_string()) {
                profiles.push(p.to_string());
            }
        }
        if profiles.len() > MAX_PROFILES {
            return Err(format!("at most {MAX_PROFILES} profiles can be shared"));
        }
        let mut types = Vec::new();
        for t in &self.note_types {
            let t = t.trim().to_lowercase();
            if !SHAREABLE_TYPES.contains(&t.as_str()) {
                return Err(format!(
                    "note type `{t}` cannot be shared (allowed: {})",
                    SHAREABLE_TYPES.join(", ")
                ));
            }
            if !types.contains(&t) {
                types.push(t);
            }
        }
        self.profiles = profiles;
        self.note_types = types;
        Ok(self)
    }
}

/// A profile folder name that cannot walk out of the profiles directory.
pub fn safe_profile(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Why a question was not answered.
#[derive(Debug, PartialEq)]
pub enum RecallError {
    /// This peer has not been allowed to read anything (or not that profile).
    NotShared,
    /// Too many questions in the last minute.
    Busy,
    Invalid(String),
}

impl RecallError {
    pub fn into_rpc(self) -> (i64, String) {
        match self {
            Self::NotShared => (a2a::KNOWLEDGE_NOT_SHARED, "this SenClaw has not shared its knowledge with you".into()),
            Self::Busy => (a2a::TOO_MANY_REQUESTS, "too many knowledge requests; try again in a minute".into()),
            Self::Invalid(m) => (a2a::INVALID_PARAMS, m),
        }
    }
}

/// How a note in a profile's `memory/` folder is treated. The one place that
/// decides what can be shared.
#[derive(Debug)]
pub enum NoteClass {
    /// A raw day log (`YYYY-MM-DD.md`): conversation text.
    DailyLog,
    /// A compaction summary of one chat.
    Summary,
    /// No frontmatter: not a curated note.
    NoMeta,
    /// About the owner; never shared.
    UserType,
    /// Written in one particular chat.
    ChatScoped,
    /// A real note, of a type the policy does not include.
    OtherType,
    Shareable(MemoryMeta),
}

pub fn classify_note(path: &Path, profile: &str, types: &[String]) -> NoteClass {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if is_daily_log_name(name) {
        return NoteClass::DailyLog;
    }
    if name.starts_with("conversation-summary") {
        return NoteClass::Summary;
    }
    let Some(meta) = read_meta(path) else {
        return NoteClass::NoMeta;
    };
    let kind = meta.mem_type.trim().to_lowercase();
    if kind == "user" {
        return NoteClass::UserType;
    }
    // Rank 1 is "for the whole agent" (no origin, or the folder's own name); a
    // chat's JID ranks otherwise and stays in that chat.
    if memory_session_rank(&meta.origin_session_id, "", profile) != 1 {
        return NoteClass::ChatScoped;
    }
    if !SHAREABLE_TYPES.contains(&kind.as_str()) || !types.iter().any(|t| *t == kind) {
        return NoteClass::OtherType;
    }
    NoteClass::Shareable(meta)
}

struct Note {
    meta: MemoryMeta,
    body: String,
}

fn body_of(content: &str) -> &str {
    content
        .strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---").map(|end| &rest[end + 4..]))
        .unwrap_or(content)
        .trim_start_matches(['\n', '\r'])
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn date_of(secs: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs, 0).map(|t| t.date_naive().to_string())
}

fn redact(text: &str) -> Option<String> {
    match scrub(text, Mode::Redact) {
        Verdict::Clean(t) | Verdict::Redacted { text: t, .. } => Some(t),
        Verdict::Blocked { .. } => None,
    }
}

/// The question as it will be searched: trimmed, cut, nothing else.
fn clean_query(raw: &str) -> String {
    raw.trim().chars().take(MAX_QUERY_CHARS).collect()
}

pub struct Knowledge {
    db: Arc<Db>,
    profiles_dir: PathBuf,
    calls: Mutex<HashMap<String, Vec<Instant>>>,
}

impl Knowledge {
    pub fn new(db: Arc<Db>, profiles_dir: PathBuf) -> Self {
        Self { db, profiles_dir, calls: Mutex::new(HashMap::new()) }
    }

    pub fn profile_exists(&self, name: &str) -> bool {
        safe_profile(name) && self.profiles_dir.join(name).is_dir()
    }

    /// The profile folders on this machine a policy could name, sorted.
    pub fn profiles(&self) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(&self.profiles_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().map_or(false, |t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| safe_profile(n))
            .collect();
        out.sort();
        out
    }

    /// Count this call against `peer`'s minute; false once it is over the limit.
    fn allow_call(&self, peer: &str, now: Instant) -> bool {
        let mut calls = self.calls.lock().unwrap();
        let recent = calls.entry(peer.to_string()).or_default();
        recent.retain(|t| now.saturating_duration_since(*t) < WINDOW);
        if recent.len() >= CALLS_PER_MINUTE {
            return false;
        }
        recent.push(now);
        true
    }

    /// Answer `knowledge/recall` for `peer` under `policy`.
    pub fn answer(&self, peer: &str, policy: &KnowledgePolicy, params: &Value) -> Result<Value, RecallError> {
        self.answer_at(peer, policy, params, Instant::now())
    }

    fn answer_at(&self, peer: &str, policy: &KnowledgePolicy, params: &Value, now: Instant) -> Result<Value, RecallError> {
        if !policy.open() {
            return Err(RecallError::NotShared);
        }
        if !self.allow_call(peer, now) {
            return Err(RecallError::Busy);
        }
        let query = clean_query(params["query"].as_str().unwrap_or_default());
        if query.is_empty() {
            return Err(RecallError::Invalid("params.query must not be empty".into()));
        }
        let limit = params["limit"].as_u64().map_or(DEFAULT_HITS, |n| n as usize).clamp(1, MAX_HITS);
        let wanted = params["profile"].as_str().map(str::trim).filter(|p| !p.is_empty());
        if let Some(p) = wanted {
            if !policy.profiles.iter().any(|x| x == p) {
                return Err(RecallError::NotShared);
            }
        }
        let profiles: Vec<&String> = policy.profiles.iter().filter(|p| wanted.map_or(true, |w| w == p.as_str())).collect();

        let mut facts: Vec<Value> = Vec::new();
        let mut notes: Vec<(u32, Value)> = Vec::new();
        if policy.facts {
            let store = FactStore::new(Arc::clone(&self.db));
            let today = chrono::Local::now().date_naive();
            for p in &profiles {
                // A bank that cannot be read costs this answer its facts, not the whole answer.
                let found = lookup(&store, p, &query, today, limit, Scope::AgentWide).unwrap_or_default();
                facts.extend(found.iter().filter_map(|f| fact_hit(p, f)));
            }
        }
        if policy.notes {
            let types = policy.effective_note_types();
            let q: HashSet<String> = tokenize_optimized(&query, true).into_iter().collect();
            for p in &profiles {
                for note in self.shareable_notes(p, &types) {
                    let score = score_note(&note, &q);
                    if score > 0 {
                        if let Some(hit) = note_hit(p, &note, &q) {
                            notes.push((score, hit));
                        }
                    }
                }
            }
            notes.sort_by(|a, b| b.0.cmp(&a.0));
        }
        let available = facts.len() + notes.len();
        // Facts and notes take turns, so one kind does not crowd out the other.
        let mut hits: Vec<Value> = Vec::new();
        let (mut f, mut n) = (facts.into_iter(), notes.into_iter().map(|(_, h)| h));
        while hits.len() < limit {
            match (f.next(), n.next()) {
                (None, None) => break,
                (a, b) => {
                    hits.extend(a);
                    if hits.len() < limit {
                        hits.extend(b);
                    }
                }
            }
        }
        hits.truncate(limit);
        let truncated = available > hits.len();
        tracing::info!(peer = %peer, hits = hits.len(), query_chars = query.chars().count(), "[slink] knowledge recall answered");
        Ok(json!({ "v": KNOWLEDGE_VERSION, "hits": hits, "truncated": truncated }))
    }

    fn memory_dir(&self, profile: &str) -> PathBuf {
        self.profiles_dir.join(profile).join("memory")
    }

    /// Every note the policy lets out of `profile`, read from disk.
    fn shareable_notes(&self, profile: &str, types: &[String]) -> Vec<Note> {
        let Ok(dir) = fs::read_dir(self.memory_dir(profile)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in dir.flatten() {
            let path = entry.path();
            // A real, small, markdown file: a link could point anywhere.
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if !meta.is_file() || meta.len() > MAX_NOTE_BYTES || path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let NoteClass::Shareable(m) = classify_note(&path, profile, types) else { continue };
            let Ok(content) = fs::read_to_string(&path) else { continue };
            out.push(Note { meta: m, body: body_of(&content).to_string() });
        }
        out
    }

    /// What a policy would let out, counted and sampled, so a person can see it
    /// before switching it on. Same rules as an answer, by construction.
    pub fn preview(&self, policy: &KnowledgePolicy) -> Value {
        let types = policy.effective_note_types();
        let mut profiles = Vec::new();
        for p in &policy.profiles {
            let mut by_type: HashMap<String, usize> = HashMap::new();
            let (mut daily, mut summaries, mut user, mut chat, mut other) = (0usize, 0usize, 0usize, 0usize, 0usize);
            let mut samples: Vec<(i64, String)> = Vec::new();
            if let Ok(dir) = fs::read_dir(self.memory_dir(p)) {
                for entry in dir.flatten() {
                    let path = entry.path();
                    let Ok(m) = fs::symlink_metadata(&path) else { continue };
                    if !m.is_file() || path.extension().and_then(|e| e.to_str()) != Some("md") {
                        continue;
                    }
                    match classify_note(&path, p, &types) {
                        NoteClass::DailyLog => daily += 1,
                        NoteClass::Summary => summaries += 1,
                        NoteClass::NoMeta => {}
                        NoteClass::UserType => user += 1,
                        NoteClass::ChatScoped => chat += 1,
                        NoteClass::OtherType => other += 1,
                        NoteClass::Shareable(meta) if m.len() <= MAX_NOTE_BYTES => {
                            *by_type.entry(meta.mem_type.trim().to_lowercase()).or_default() += 1;
                            samples.push((meta.created_at.unwrap_or(0), meta.name));
                        }
                        NoteClass::Shareable(_) => {}
                    }
                }
            }
            samples.sort_by(|a, b| b.0.cmp(&a.0));
            let facts = self.agent_wide_fact_count(p);
            profiles.push(json!({
                "profile": p,
                "exists": self.profile_exists(p),
                "facts": if policy.facts { facts } else { 0 },
                "notes": if policy.notes { json!(by_type) } else { json!({}) },
                "samples": samples.into_iter().take(5).map(|s| clip(&s.1, 80)).collect::<Vec<_>>(),
                "excluded": {
                    "dailyLogs": daily, "conversationSummaries": summaries, "aboutTheOwner": user,
                    "writtenInOneChat": chat, "otherTypes": other,
                },
            }));
        }
        json!({ "open": policy.open(), "profiles": profiles })
    }

    fn agent_wide_fact_count(&self, bank: &str) -> usize {
        self.db
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM mem_facts WHERE bank = ?1 AND origin = '' AND superseded_by IS NULL",
                    [bank],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0) as usize)
            })
            .unwrap_or(0)
    }
}

fn fact_hit(profile: &str, f: &StoredFact) -> Option<Value> {
    let text = clip(&redact(&f.text)?, MAX_TEXT_CHARS);
    Some(json!({
        "kind": "fact",
        "profile": profile,
        "text": text,
        "when": date_of(f.anchor()),
    }))
}

fn score_note(note: &Note, q: &HashSet<String>) -> u32 {
    if q.is_empty() {
        return 0;
    }
    let head: HashSet<String> = tokenize_optimized(&format!("{} {}", note.meta.name, note.meta.description), true).into_iter().collect();
    let body_text: String = note.body.chars().take(MAX_SCORED_CHARS).collect();
    let body: HashSet<String> = tokenize_optimized(&body_text, true).into_iter().collect();
    q.iter().map(|t| 3 * u32::from(head.contains(t)) + u32::from(body.contains(t))).sum()
}

/// The note's description, then the one or two body lines that match the question best.
fn note_hit(profile: &str, note: &Note, q: &HashSet<String>) -> Option<Value> {
    let mut lines: Vec<(usize, &str)> = note
        .body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| (tokenize_optimized(l, true).iter().filter(|t| q.contains(*t)).count(), l))
        .collect();
    lines.sort_by(|a, b| b.0.cmp(&a.0));
    let excerpt: Vec<&str> = lines.iter().filter(|l| l.0 > 0).take(2).map(|l| l.1).collect();
    let mut text = note.meta.description.trim().to_string();
    for l in excerpt {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(l);
    }
    let text = clip(&redact(&text)?, MAX_TEXT_CHARS);
    Some(json!({
        "kind": "note",
        "profile": profile,
        "title": clip(&note.meta.name, 120),
        "type": note.meta.mem_type.trim().to_lowercase(),
        "text": text,
        "when": note.meta.created_at.and_then(date_of),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::memory::engine::facts::store::{entity_key, FactKind, NewFact};

    struct Fixture {
        _dir: tempfile::TempDir,
        k: Knowledge,
        facts: FactStore,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open_in_memory(&Config::from_env()).unwrap());
        let facts = FactStore::new(Arc::clone(&db));
        let k = Knowledge::new(db, dir.path().to_path_buf());
        Fixture { _dir: dir, k, facts }
    }

    fn note(fx: &Fixture, profile: &str, file: &str, front: &str, body: &str) {
        let dir = fx.k.memory_dir(profile);
        fs::create_dir_all(&dir).unwrap();
        let content = if front.is_empty() { body.to_string() } else { format!("---\n{front}\n---\n\n{body}\n") };
        fs::write(dir.join(file), content).unwrap();
    }

    fn front(name: &str, kind: &str, origin: &str) -> String {
        format!("name: {name}\ndescription: \"{name} description\"\nmetadata:\n  type: {kind}\n  originSessionId: {origin}\n  createdAt: 2026-07-12")
    }

    fn fact(text: &str, origin: &str) -> NewFact {
        NewFact {
            text: text.into(),
            kind: FactKind::World,
            occurred: None,
            mentioned_at: 1_000,
            entities: vec![entity_key("office")],
            embedding: None,
            model: None,
            source: "test".into(),
            origin: origin.into(),
            slot: None,
        }
    }

    fn policy(facts: bool, notes: bool, profiles: &[&str]) -> KnowledgePolicy {
        KnowledgePolicy { facts, notes, profiles: profiles.iter().map(|p| p.to_string()).collect(), note_types: vec![] }
    }

    fn ask(fx: &Fixture, p: &KnowledgePolicy, query: &str) -> Result<Value, RecallError> {
        fx.k.answer("peer-1", p, &json!({ "query": query }))
    }

    fn titles(v: &Value) -> Vec<String> {
        v["hits"].as_array().unwrap().iter().map(|h| h["title"].as_str().or(h["text"].as_str()).unwrap_or_default().to_string()).collect()
    }

    #[test]
    fn nothing_is_shared_until_a_person_says_so() {
        let fx = fixture();
        fx.facts.insert("main", &fact("The office wifi is called Hanoi5", "")).unwrap();
        note(&fx, "main", "wifi.md", &front("wifi", "reference", "main"), "The office wifi password rotates monthly");
        assert_eq!(ask(&fx, &KnowledgePolicy::default(), "office wifi").unwrap_err(), RecallError::NotShared);
        // Switches on but no profile named, and a profile named but both switches off: still nothing.
        assert_eq!(ask(&fx, &policy(true, true, &[]), "office wifi").unwrap_err(), RecallError::NotShared);
        assert_eq!(ask(&fx, &policy(false, false, &["main"]), "office wifi").unwrap_err(), RecallError::NotShared);
        let both = ask(&fx, &policy(true, true, &["main"]), "office wifi").unwrap();
        assert_eq!(both["hits"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn only_whole_agent_facts_of_the_named_profiles_come_out() {
        let fx = fixture();
        fx.facts.insert("main", &fact("The office lease ends in March", "")).unwrap();
        fx.facts.insert("main", &fact("The office alarm code was shared in the family chat", "web:family")).unwrap();
        fx.facts.insert("other", &fact("The office in Da Nang opens at nine", "")).unwrap();
        let got = ask(&fx, &policy(true, false, &["main"]), "office").unwrap();
        let hits = got["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{got}");
        assert!(hits[0]["text"].as_str().unwrap().contains("lease"));
        assert_eq!(hits[0]["profile"], "main");
        // Asking for a profile the policy does not name is the same refusal as having no policy.
        let err = fx.k.answer("peer-1", &policy(true, false, &["main"]), &json!({ "query": "office", "profile": "other" })).unwrap_err();
        assert_eq!(err, RecallError::NotShared);
    }

    #[test]
    fn each_kind_of_note_that_must_stay_home_stays_home() {
        let fx = fixture();
        note(&fx, "main", "plan.md", &front("launch plan", "project", "main"), "The launch plan needs a budget review");
        note(&fx, "main", "2026-06-03.md", "", "The launch plan was discussed in the transcript with a customer");
        note(&fx, "main", "conversation-summary-1-2-3.md", &front("launch summary", "project", "main"), "launch plan summary of one chat");
        note(&fx, "main", "owner.md", &front("owner launch", "user", "main"), "The owner's launch plan notes about himself");
        note(&fx, "main", "chat.md", &front("chat launch", "project", "web:family"), "launch plan written in the family chat");
        note(&fx, "main", "plain.md", "", "launch plan note with no frontmatter at all");
        note(&fx, "main", "fb.md", &front("launch feedback", "feedback", "main"), "launch plan feedback not in the default types");
        let got = ask(&fx, &policy(false, true, &["main"]), "launch plan").unwrap();
        assert_eq!(titles(&got), vec!["launch plan".to_string()], "{got}");

        // The preview counts the same classes the answer left out.
        let pv = fx.k.preview(&policy(false, true, &["main"]));
        let p = &pv["profiles"][0];
        assert_eq!(p["notes"]["project"], 1);
        assert_eq!(p["excluded"]["dailyLogs"], 1);
        assert_eq!(p["excluded"]["conversationSummaries"], 1);
        assert_eq!(p["excluded"]["aboutTheOwner"], 1);
        assert_eq!(p["excluded"]["writtenInOneChat"], 1);
        assert_eq!(p["excluded"]["otherTypes"], 1);
        assert_eq!(p["samples"][0], "launch plan");
    }

    #[test]
    fn a_person_can_add_feedback_notes_but_never_the_owners_own() {
        let fx = fixture();
        note(&fx, "main", "fb.md", &front("launch feedback", "feedback", "main"), "launch plan feedback");
        let mut p = policy(false, true, &["main"]);
        p.note_types = vec!["feedback".into()];
        assert_eq!(titles(&ask(&fx, &p, "launch plan").unwrap()), vec!["launch feedback".to_string()]);
        let rejected = KnowledgePolicy { note_types: vec!["user".into()], ..policy(false, true, &["main"]) };
        assert!(rejected.validated(|_| true).unwrap_err().contains("cannot be shared"));
    }

    #[test]
    fn a_policy_names_only_real_safe_profile_folders() {
        let exists = |p: &str| p == "main" || p == "ssh";
        let ok = policy(true, true, &[" main ", "ssh", "main"]).validated(exists).unwrap();
        assert_eq!(ok.profiles, vec!["main".to_string(), "ssh".to_string()]);
        for bad in ["../etc", "a/b", ".hidden", "", "x y", "nope"] {
            assert!(policy(true, true, &[bad]).validated(exists).is_err(), "{bad:?}");
        }
        let many: Vec<String> = (0..40).map(|i| format!("p{i}")).collect();
        let big = KnowledgePolicy { facts: true, profiles: many, ..Default::default() };
        assert!(big.validated(|_| true).unwrap_err().contains("at most"));
    }

    #[test]
    fn answers_are_small_and_a_secret_in_a_note_does_not_travel() {
        let fx = fixture();
        let long = "alpha ".repeat(400);
        note(&fx, "main", "long.md", &front("alpha notes", "reference", "main"), &format!("{long}\nkey AKIAIOSFODNN7EXAMPLE alpha"));
        let got = ask(&fx, &policy(false, true, &["main"]), "alpha").unwrap();
        let text = got["hits"][0]["text"].as_str().unwrap();
        assert!(text.chars().count() <= MAX_TEXT_CHARS, "{}", text.chars().count());
        assert!(!text.contains("AKIAIOSFODNN7EXAMPLE"), "{text}");

        for i in 0..15 {
            note(&fx, "main", &format!("n{i}.md"), &front(&format!("beta {i}"), "reference", "main"), "beta beta");
        }
        let many = fx.k.answer("peer-2", &policy(false, true, &["main"]), &json!({ "query": "beta", "limit": 500 })).unwrap();
        assert_eq!(many["hits"].as_array().unwrap().len(), MAX_HITS);
        assert_eq!(many["truncated"], true);
        let one = fx.k.answer("peer-2", &policy(false, true, &["main"]), &json!({ "query": "beta", "limit": 0 })).unwrap();
        assert_eq!(one["hits"].as_array().unwrap().len(), 1, "a limit of 0 is raised to 1");
    }

    #[test]
    fn facts_and_notes_take_turns_and_an_empty_question_is_refused() {
        let fx = fixture();
        for i in 0..3 {
            fx.facts.insert("main", &fact(&format!("gamma fact number {i}"), "")).unwrap();
            note(&fx, "main", &format!("g{i}.md"), &front(&format!("gamma note {i}"), "reference", "main"), "gamma body");
        }
        let got = fx.k.answer("peer-3", &policy(true, true, &["main"]), &json!({ "query": "gamma", "limit": 4 })).unwrap();
        let kinds: Vec<&str> = got["hits"].as_array().unwrap().iter().map(|h| h["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["fact", "note", "fact", "note"]);
        assert!(matches!(ask(&fx, &policy(true, true, &["main"]), "   "), Err(RecallError::Invalid(_))));
    }

    #[test]
    fn a_peer_gets_thirty_questions_a_minute() {
        let fx = fixture();
        let p = policy(true, false, &["main"]);
        let t0 = Instant::now();
        for _ in 0..CALLS_PER_MINUTE {
            fx.k.answer_at("peer-4", &p, &json!({ "query": "x" }), t0).unwrap();
        }
        assert_eq!(fx.k.answer_at("peer-4", &p, &json!({ "query": "x" }), t0).unwrap_err(), RecallError::Busy);
        // Another peer is not slowed, and the first one is let back in after the window.
        fx.k.answer_at("peer-5", &p, &json!({ "query": "x" }), t0).unwrap();
        fx.k.answer_at("peer-4", &p, &json!({ "query": "x" }), t0 + WINDOW + Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn a_note_that_is_a_link_or_too_big_is_not_read() {
        let fx = fixture();
        let dir = fx.k.memory_dir("main");
        fs::create_dir_all(&dir).unwrap();
        let outside = fx._dir.path().join("secret.md");
        fs::write(&outside, format!("---\n{}\n---\n\ndelta secret", front("delta secret", "reference", "main"))).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, dir.join("link.md")).unwrap();
        note(&fx, "main", "huge.md", &front("delta huge", "reference", "main"), &"delta ".repeat(60_000));
        let got = ask(&fx, &policy(false, true, &["main"]), "delta").unwrap();
        assert!(got["hits"].as_array().unwrap().is_empty(), "{got}");
    }

    #[test]
    fn the_policy_round_trips_with_camel_case_and_defaults_to_closed() {
        let p: KnowledgePolicy = serde_json::from_value(json!({ "facts": true, "noteTypes": ["project"], "profiles": ["main"] })).unwrap();
        assert!(p.facts && !p.notes && p.open());
        assert_eq!(serde_json::to_value(&p).unwrap()["noteTypes"], json!(["project"]));
        let empty: KnowledgePolicy = serde_json::from_value(json!({})).unwrap();
        assert!(!empty.open());
        assert_eq!(empty.effective_note_types(), vec!["project".to_string(), "reference".to_string()]);
    }

    // ---- over the real sealed channel -------------------------------------------------------

    use crate::sen_link::testkit::{node, node_with, paired, paired_at, tap, Node};
    use crate::sen_link::{api_router, client};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// A daemon that knows things: one agent-wide fact and one reference note in profile `main`.
    async fn knowing_node() -> (Node, tempfile::TempDir) {
        let profiles = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open_in_memory(&Config::from_env()).unwrap());
        FactStore::new(Arc::clone(&db)).insert("main", &fact("The harbour office lease ends in March", "")).unwrap();
        let mem = profiles.path().join("main").join("memory");
        fs::create_dir_all(&mem).unwrap();
        fs::write(mem.join("harbour.md"), format!("---\n{}\n---\n\nHarbour berth fees are paid quarterly\n", front("harbour fees", "reference", "main"))).unwrap();
        let dir = profiles.path().to_path_buf();
        let n = node_with("Knower", json!({}), move |st| st.with_knowledge(db, dir)).await;
        (n, profiles)
    }

    fn open_to(n: &Node, peer_id: &str, p: KnowledgePolicy) {
        n.st.store.update(peer_id, |peer| peer.knowledge = p).unwrap().unwrap();
    }

    async fn rest(n: &Node, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        let resp = api_router(Arc::clone(&n.st)).oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn a_peer_gets_nothing_until_it_is_allowed_and_then_only_what_the_policy_names() {
        let (b, _profiles) = knowing_node().await;
        let a = node("Asker", json!({})).await;
        let (at_a, at_b) = paired(&a, &b).await;
        let ask = |q: &'static str| {
            let (st, peer) = (Arc::clone(&a.st), at_a.clone());
            async move { client::call(&st, &peer, METHOD_RECALL, json!({ "query": q })).await }
        };

        let closed = ask("harbour").await.unwrap_err().to_string();
        assert!(closed.contains(&format!("({})", a2a::KNOWLEDGE_NOT_SHARED)), "{closed}");

        open_to(&b, &at_b.id, policy(true, true, &["main"]));
        let got = ask("harbour").await.unwrap();
        let kinds: Vec<&str> = got["hits"].as_array().unwrap().iter().map(|h| h["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["fact", "note"], "{got}");

        // Narrow it to facts only: the note is gone from the very next answer.
        open_to(&b, &at_b.id, policy(true, false, &["main"]));
        let facts_only = ask("harbour").await.unwrap();
        assert_eq!(facts_only["hits"].as_array().unwrap().len(), 1);

        // And closing it again closes it.
        open_to(&b, &at_b.id, KnowledgePolicy::default());
        assert!(ask("harbour").await.is_err());
    }

    #[tokio::test]
    async fn a_daemon_that_cannot_share_answers_like_an_older_one() {
        let a = node("Asker", json!({})).await;
        let b = node("Plain", json!({})).await;
        let (at_a, _) = paired(&a, &b).await;
        let e = client::call(&a.st, &at_a, METHOD_RECALL, json!({ "query": "x" })).await.unwrap_err().to_string();
        assert!(e.contains("(-32601)"), "{e}");
    }

    #[tokio::test]
    async fn neither_the_question_nor_the_answer_can_be_read_on_the_wire() {
        let (b, _profiles) = knowing_node().await;
        let a = node("Asker", json!({})).await;
        let relay = tap(b.port).await;
        let (at_a, at_b) = paired_at(&a, &b, Some(format!("127.0.0.1:{}", relay.port))).await;
        open_to(&b, &at_b.id, policy(true, true, &["main"]));
        let got = client::call(&a.st, &at_a, METHOD_RECALL, json!({ "query": "harbour berth lease" })).await.unwrap();
        assert!(got["hits"].as_array().unwrap().len() >= 2, "{got}");
        let seen = relay.text();
        assert!(seen.contains("/a2a"), "the relay saw the traffic it is checking");
        for secret in ["harbour", "berth", "lease", "quarterly", "March", "reference"] {
            assert!(!seen.contains(secret), "{secret:?} crossed the network in the clear");
        }
    }

    #[tokio::test]
    async fn the_policy_is_set_and_previewed_over_rest_and_a_bad_one_changes_nothing() {
        let (b, profiles) = knowing_node().await;
        let a = node("Asker", json!({})).await;
        let (_, at_b) = paired(&a, &b).await;
        let uri = format!("/api/sen-link/peers/{}", at_b.id);

        let (code, view) = rest(&b, "GET", &format!("{uri}/knowledge"), None).await;
        assert_eq!(code, 200);
        assert_eq!(view["policy"]["facts"], false);
        assert_eq!(view["profiles"], json!(["main"]));
        assert_eq!(view["preview"]["open"], false);

        let (code, patched) = rest(&b, "PATCH", &uri, Some(json!({ "knowledge": { "facts": true, "notes": true, "profiles": ["main"] } }))).await;
        assert_eq!(code, 200, "{patched}");
        assert_eq!(patched["knowledge"]["profiles"], json!(["main"]));
        let (_, view) = rest(&b, "GET", &format!("{uri}/knowledge"), None).await;
        assert_eq!(view["preview"]["profiles"][0]["facts"], 1);
        assert_eq!(view["preview"]["profiles"][0]["notes"]["reference"], 1);
        assert_eq!(view["preview"]["profiles"][0]["samples"][0], "harbour fees");

        // A profile that is not there, a type that must never leave, a path trick: all refused, nothing changed.
        for bad in [
            json!({ "knowledge": { "profiles": ["nope"], "facts": true } }),
            json!({ "knowledge": { "profiles": ["main"], "notes": true, "noteTypes": ["user"] } }),
            json!({ "knowledge": { "profiles": ["../main"], "facts": true } }),
        ] {
            let (code, body) = rest(&b, "PATCH", &uri, Some(bad)).await;
            assert_eq!(code, 400, "{body}");
        }
        assert_eq!(b.st.store.get(&at_b.id).unwrap().knowledge.profiles, vec!["main".to_string()]);
        assert!(profiles.path().join("main").is_dir());
    }

    #[tokio::test]
    async fn the_asking_side_gets_a_clear_message_and_words_marked_as_from_elsewhere() {
        let (b, _profiles) = knowing_node().await;
        let a = node("Asker", json!({})).await;
        let (at_a, at_b) = paired(&a, &b).await;
        async fn ask(a: &Node, peer: &str, q: &str) -> (u16, Value) {
            rest(a, "POST", &format!("/api/sen-link/peers/{peer}/recall"), Some(json!({ "query": q }))).await
        }

        let (code, body) = ask(&a, &at_a.id, "harbour").await;
        assert_eq!(code, 502);
        assert!(body["error"].as_str().unwrap().contains("has not shared its knowledge"), "{body}");

        open_to(&b, &at_b.id, policy(true, true, &["main"]));
        let (code, body) = ask(&a, &at_a.id, "harbour").await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["untrusted"], true);
        assert_eq!(body["from"], "Knower");
        assert_eq!(rest(&a, "POST", &format!("/api/sen-link/peers/{}/recall", at_a.id), Some(json!({ "query": "  " }))).await.0, 400);

        let plain = node("Plain", json!({})).await;
        let (at_a2, _) = paired(&a, &plain).await;
        let (code, body) = rest(&a, "POST", &format!("/api/sen-link/peers/{}/recall", at_a2.id), Some(json!({ "query": "x" }))).await;
        assert_eq!(code, 502);
        assert!(body["error"].as_str().unwrap().contains("update SenClaw"), "{body}");
    }
}

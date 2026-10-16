//! Turning a conversation into facts: the prompt, and a parser lenient enough
//! for a small local model.
//!
//! The prompt follows Hindsight's concise fact extraction (selective, absolute
//! dates, pronouns resolved, one self-contained statement per fact, `world`
//! vs `experience` decided by who is speaking) with this daemon's Vietnamese
//! guidance. The reply shape is deliberately flat — four fields — because the
//! extractor is often a 4B model that is configured to be cheap, and an output
//! it cannot produce reliably is worse than a poorer one it can.

use anyhow::{anyhow, Result};
use chrono::{Datelike, NaiveDate};
use serde_json::Value;

use super::store::{entity_key, FactKind};
use crate::memory::cognitive::llm::json_object_slice;

/// A fact longer than this is a paragraph the model failed to split.
const MAX_FACT_CHARS: usize = 600;
/// Shorter than this is a fragment ("Sen.").
const MIN_FACT_CHARS: usize = 8;
const MAX_FACTS_PER_CHUNK: usize = 24;
const MAX_ENTITIES_PER_FACT: usize = 8;
const MAX_ENTITY_CHARS: usize = 80;

pub const SYSTEM_PROMPT: &str = r#"You extract durable memories from a conversation or document, so an assistant can recall them months from now.

Reply with ONLY a JSON object, nothing before or after it:
{"facts":[{"text":"...","kind":"world","when":"2026-06-09","entities":["Emily","Sarah"]}]}
Reply {"facts":[]} when nothing is worth keeping.

SELECTIVITY — keep only:
- who people are, their relationships, roles, background
- preferences, habits, rules and corrections the user states
- decisions, plans, deadlines, commitments
- events with a date, milestones, changes
- skills and expertise; project context, problems, constraints
- what the assistant itself did, tried, decided, recommended or learned
Skip: greetings, thanks, filler ("ok", "got it", "vâng"), process chatter ("let me check"), anything already said, questions that carry no information, and any secret or token (text like [REDACTED:...]).
Ask: "would this still be useful to recall in six months?" If not, skip it.

EACH FACT
- "text": one or two sentences that make sense alone. Say who, what, and where or why when known. Never leave "he", "she", "it", "they", "this" unresolved — use the name, taking it from earlier lines ("my roommate" + "Emily" becomes "Emily, the user's roommate").
- "kind": "world" for facts about the user, other people, places, things — including the user's own preferences, rules and corrections, even when said to the assistant. "experience" only for what the assistant itself did, tried, decided, recommended or learned.
- "when": when the event happened, as "YYYY-MM-DD", "YYYY-MM" or "YYYY" (the precision the source gives; never invent a day). Convert relative dates ("yesterday", "hôm qua", "last Friday", "tuần trước") using TODAY. null for a standing fact or preference with no date.
- "entities": plain strings — people, organizations, places, projects, products, key concepts, in their original spelling. Include "user" when the fact is about the user.

LANGUAGE: write every fact in the language of the input. Never translate. Names, numbers, identifiers and quoted text stay exactly as written.

SPEAKERS: lines look like "Name: text". A first-person statement belongs to its speaker. "User: tôi tên là Sen" means the user's name is Sen.

Example (TODAY: 2026-06-10)
INPUT:
User: Hi! I'm planning my own wedding, a small outdoor ceremony.
Assistant: Congratulations! I can help with a checklist.
User: Yesterday I went to Emily's wedding. She married Sarah on a rooftop. Anyway, thanks!
OUTPUT:
{"facts":[{"text":"The user is planning their own wedding and wants a small outdoor ceremony.","kind":"world","when":null,"entities":["user","wedding"]},{"text":"Emily, a friend of the user, married Sarah at a rooftop garden on 2026-06-09.","kind":"world","when":"2026-06-09","entities":["Emily","Sarah","user"]}]}

Example (TODAY: 2026-10-16)
INPUT:
User: tôi tên là Sen, đang làm dự án SemaClaw, deadline cuối tháng 8 năm 2026
Assistant: Đã ghi nhận.
OUTPUT:
{"facts":[{"text":"Người dùng tên là Sen.","kind":"world","when":null,"entities":["Sen","user"]},{"text":"Sen đang làm dự án SemaClaw, hạn chót là cuối tháng 8 năm 2026.","kind":"world","when":"2026-08","entities":["Sen","SemaClaw"]}]}"#;

/// The user message: the date that relative phrases resolve against, then the
/// text.
pub fn user_prompt(text: &str, today: NaiveDate) -> String {
    format!(
        "TODAY: {}\n\nINPUT:\n{}",
        today.format("%Y-%m-%d"),
        text.trim()
    )
}

/// One fact as the model reported it, validated and normalised.
#[derive(Debug, Clone, PartialEq)]
pub struct Extracted {
    pub text: String,
    pub kind: FactKind,
    /// Unix seconds, `[start, end)`, from `when`; `None` when undated.
    pub occurred: Option<(i64, i64)>,
    /// [`entity_key`]s, de-duplicated.
    pub entities: Vec<String>,
}

/// Parse a model reply into facts. An unparseable reply is an `Err`; a parsed
/// reply with no usable facts is `Ok(vec![])` — "nothing worth keeping" is a
/// legitimate answer, not a failure.
pub fn parse_facts(raw: &str) -> Result<Vec<Extracted>> {
    let items = facts_array(raw)?;
    let mut out: Vec<Extracted> = Vec::new();
    for item in items.iter().take(MAX_FACTS_PER_CHUNK * 2) {
        if let Some(f) = read_fact(item) {
            if !out.iter().any(|o| o.text == f.text) {
                out.push(f);
            }
        }
        if out.len() >= MAX_FACTS_PER_CHUNK {
            break;
        }
    }
    Ok(out)
}

fn facts_array(raw: &str) -> Result<Vec<Value>> {
    // `{"facts":[...]}` is what was asked for; a bare array is what some
    // models answer with instead.
    if let Ok(v) = serde_json::from_str::<Value>(json_object_slice(raw)) {
        match v {
            Value::Object(mut o) => {
                if let Some(Value::Array(a)) = o.remove("facts") {
                    return Ok(a);
                }
            }
            Value::Array(a) => return Ok(a),
            _ => {}
        }
    }
    // A bare array is accepted only when the reply *is* one (optionally
    // fenced). Looking for any `[` would find the empty list inside
    // `{"triplets":[]}` and read a reply in the wrong shape as "no facts".
    let body = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim();
    if body.starts_with('[') {
        if let Some(end) = body.rfind(']') {
            if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&body[..=end]) {
                return Ok(a);
            }
        }
    }
    Err(anyhow!(
        "fact JSON not found in reply: {}",
        crate::util::text::truncate_on_char_boundary(raw.trim(), 200)
    ))
}

fn read_fact(item: &Value) -> Option<Extracted> {
    let obj = item.as_object()?;
    let text = obj
        .get("text")
        .or_else(|| obj.get("fact"))?
        .as_str()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let len = text.chars().count();
    if !(MIN_FACT_CHARS..=MAX_FACT_CHARS).contains(&len) {
        return None;
    }
    // The extractor was told to skip secrets; one that talks about a redacted
    // token anyway is storing a hole.
    if text.contains("[REDACTED") {
        return None;
    }
    let kind = obj
        .get("kind")
        .or_else(|| obj.get("fact_type"))
        .and_then(Value::as_str)
        .and_then(FactKind::parse)
        .unwrap_or(FactKind::World);
    let occurred = obj.get("when").and_then(Value::as_str).and_then(parse_when);

    let mut entities: Vec<String> = Vec::new();
    if let Some(Value::Array(list)) = obj.get("entities") {
        for e in list {
            // Some models wrap each name: {"text": "Emily"}.
            let name = e
                .as_str()
                .or_else(|| e.get("text").and_then(Value::as_str))
                .or_else(|| e.get("name").and_then(Value::as_str));
            let Some(name) = name else { continue };
            let key = entity_key(name);
            if key.is_empty() || key.chars().count() > MAX_ENTITY_CHARS || entities.contains(&key) {
                continue;
            }
            entities.push(key);
            if entities.len() >= MAX_ENTITIES_PER_FACT {
                break;
            }
        }
    }
    Some(Extracted {
        text,
        kind,
        occurred,
        entities,
    })
}

/// `YYYY-MM-DD`, `YYYY-MM` or `YYYY` (a time after the day is ignored), as the
/// span of that day, month or year. A coarse date covers the *whole* period —
/// "in March 2026" is not "on 1 March" — and a range `A/B` runs from the start
/// of `A` to the end of `B`.
pub fn parse_when(s: &str) -> Option<(i64, i64)> {
    let s = s.trim();
    let range = s.split_once('/').or_else(|| s.split_once(".."));
    if let Some((a, b)) = range {
        let (start, _) = parse_one(a)?;
        let (_, end) = parse_one(b)?;
        return (end > start).then_some((start, end));
    }
    parse_one(s)
}

fn parse_one(s: &str) -> Option<(i64, i64)> {
    let s = s.trim();
    let date_part = s.split(['T', ' ']).next().unwrap_or(s);
    let mut parts = date_part.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    if !(1900..=2200).contains(&year) {
        return None;
    }
    let month: Option<u32> = parts.next().and_then(|m| m.parse().ok());
    let day: Option<u32> = parts.next().and_then(|d| d.parse().ok());
    if parts.next().is_some() {
        return None;
    }
    let (start, end) = match (month, day) {
        (Some(m), Some(d)) => {
            let start = NaiveDate::from_ymd_opt(year, m, d)?;
            (start, start.succ_opt()?)
        }
        (Some(m), None) => {
            let start = NaiveDate::from_ymd_opt(year, m, 1)?;
            let end = if m == 12 {
                NaiveDate::from_ymd_opt(year + 1, 1, 1)?
            } else {
                NaiveDate::from_ymd_opt(year, m + 1, 1)?
            };
            (start, end)
        }
        (None, _) => (
            NaiveDate::from_ymd_opt(year, 1, 1)?,
            NaiveDate::from_ymd_opt(year + 1, 1, 1)?,
        ),
    };
    Some((midnight(start), midnight(end)))
}

pub(crate) fn midnight(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc()
        .timestamp()
}

/// The precision of a stored span, for display: `"2026"`, `"2026-03"` or
/// `"2026-03-09"`.
pub fn span_label(start: i64, end: i64) -> String {
    let Some(s) = chrono::DateTime::from_timestamp(start, 0).map(|d| d.date_naive()) else {
        return String::new();
    };
    let e = chrono::DateTime::from_timestamp(end, 0).map(|d| d.date_naive());
    let whole_year = s.month() == 1
        && s.day() == 1
        && e.is_some_and(|e| e == NaiveDate::from_ymd_opt(s.year() + 1, 1, 1).unwrap_or(e));
    let whole_month = s.day() == 1
        && e.is_some_and(|e| {
            let next = if s.month() == 12 {
                NaiveDate::from_ymd_opt(s.year() + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(s.year(), s.month() + 1, 1)
            };
            next == Some(e)
        });
    if whole_year {
        format!("{}", s.year())
    } else if whole_month {
        s.format("%Y-%m").to_string()
    } else {
        s.format("%Y-%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> (i64, i64) {
        let s = NaiveDate::from_ymd_opt(y, m, d).unwrap();
        (midnight(s), midnight(s.succ_opt().unwrap()))
    }

    #[test]
    fn a_well_formed_reply_becomes_facts() {
        let raw = r#"{"facts":[
            {"text":"Emily, a friend of the user, married Sarah on 2026-06-09.","kind":"world","when":"2026-06-09","entities":["Emily","Sarah","user"]},
            {"text":"I fixed the login bug by clearing the stale cookie.","kind":"experience","when":null,"entities":["login bug"]}]}"#;
        let facts = parse_facts(raw).unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].kind, FactKind::World);
        assert_eq!(facts[0].occurred, Some(day(2026, 6, 9)));
        assert_eq!(facts[0].entities, vec!["emily", "sarah", "user"]);
        assert_eq!(facts[1].kind, FactKind::Experience);
        assert_eq!(facts[1].occurred, None);
    }

    #[test]
    fn prose_fences_and_bare_arrays_are_tolerated() {
        let body = r#"{"facts":[{"text":"The user prefers dark mode everywhere.","kind":"world","when":null,"entities":["user"]}]}"#;
        for raw in [
            format!("Sure! Here you go:\n{body}\nHope that helps."),
            format!("```json\n{body}\n```"),
            r#"[{"text":"The user prefers dark mode everywhere.","entities":["user"]}]"#
                .to_string(),
        ] {
            let facts = parse_facts(&raw).unwrap();
            assert_eq!(facts.len(), 1, "{raw}");
            assert_eq!(facts[0].text, "The user prefers dark mode everywhere.");
        }
    }

    #[test]
    fn nothing_worth_keeping_is_an_answer_not_a_failure() {
        assert_eq!(parse_facts(r#"{"facts":[]}"#).unwrap(), vec![]);
        assert!(parse_facts("I could not find anything.").is_err());
        assert!(parse_facts("").is_err());
        assert!(
            parse_facts(r#"{"triplets":[]}"#).is_err(),
            "a reply in the wrong shape is a failure"
        );
    }

    #[test]
    fn unusable_facts_are_dropped_and_the_rest_kept() {
        let raw = r#"{"facts":[
            {"text":"Sen.","kind":"world"},
            {"text":"The deploy token is [REDACTED:api_key] and was rotated yesterday.","kind":"world"},
            {"kind":"world"},
            "just a string",
            {"text":"The user's name is Sen and they live in Hanoi.","kind":"world","entities":["Sen"]},
            {"text":"The user's name is Sen and they live in Hanoi.","kind":"world"}]}"#;
        let facts = parse_facts(raw).unwrap();
        assert_eq!(
            facts.len(),
            1,
            "fragments, redactions, malformed items and repeats are out"
        );
    }

    #[test]
    fn kind_defaults_to_world_and_accepts_the_other_spelling() {
        let raw = r#"{"facts":[
            {"text":"The assistant recommended Postgres for the new service."},
            {"text":"The assistant renamed the migration folder yesterday.","fact_type":"assistant"},
            {"text":"Something the model labelled strangely about Bob.","kind":"opinion"}]}"#;
        let f = parse_facts(raw).unwrap();
        assert_eq!(f[0].kind, FactKind::World);
        assert_eq!(f[1].kind, FactKind::Experience);
        assert_eq!(f[2].kind, FactKind::World);
    }

    #[test]
    fn entities_are_normalised_wrapped_names_unwrapped_and_the_list_bounded() {
        let raw = r#"{"facts":[{"text":"Alice and Bob met at the Hà Nội office last year.","entities":
            ["Alice","  ALICE ", {"text":"Bob"}, {"name":"Hà   Nội"}, 7, null, "", "e1","e2","e3","e4","e5","e6","e7"]}]}"#;
        let f = parse_facts(raw).unwrap();
        assert_eq!(&f[0].entities[..4], ["alice", "bob", "hà nội", "e1"]);
        assert_eq!(f[0].entities.len(), MAX_ENTITIES_PER_FACT);
    }

    #[test]
    fn a_dateless_or_garbled_when_is_simply_undated() {
        let raw = r#"{"facts":[
            {"text":"The user likes green tea in the evening.","when":"N/A"},
            {"text":"The user likes green tea in the morning.","when":"someday"},
            {"text":"The user likes green tea at night, always.","when":2026}]}"#;
        for f in parse_facts(raw).unwrap() {
            assert_eq!(f.occurred, None);
        }
    }

    #[test]
    fn a_coarse_date_covers_the_whole_period() {
        assert_eq!(parse_when("2026-06-09"), Some(day(2026, 6, 9)));
        let (s, e) = parse_when("2026-02").unwrap();
        assert_eq!(e - s, 28 * 86_400);
        let (s, e) = parse_when("2026").unwrap();
        assert_eq!(e - s, 365 * 86_400);
        let (s, e) = parse_when("2024").unwrap();
        assert_eq!(e - s, 366 * 86_400, "a leap year is a day longer");
        let (s, e) = parse_when("2026-12").unwrap();
        assert_eq!(e - s, 31 * 86_400, "December ends at the new year");
        assert_eq!(parse_when("2026-06-09T10:30:00Z"), Some(day(2026, 6, 9)));
    }

    #[test]
    fn a_range_runs_from_the_start_of_the_first_to_the_end_of_the_last() {
        let (s, e) = parse_when("2026-03-01/2026-03-15").unwrap();
        assert_eq!(s, day(2026, 3, 1).0);
        assert_eq!(e, day(2026, 3, 15).1);
        assert_eq!(
            parse_when("2026-03-15/2026-03-01"),
            None,
            "backwards is not a range"
        );
    }

    #[test]
    fn impossible_dates_are_rejected() {
        for bad in [
            "2026-02-30",
            "2026-13",
            "1500",
            "2026-06-09-01",
            "",
            "yesterday",
            "26-06-09",
        ] {
            assert_eq!(parse_when(bad), None, "{bad}");
        }
    }

    #[test]
    fn spans_print_at_the_precision_they_were_given() {
        for s in ["2026", "2026-03", "2026-03-09"] {
            let (a, b) = parse_when(s).unwrap();
            assert_eq!(span_label(a, b), s);
        }
        let (a, b) = parse_when("2026-12").unwrap();
        assert_eq!(span_label(a, b), "2026-12");
    }

    #[test]
    fn the_prompt_carries_today_and_the_text_and_demands_json_only() {
        let p = user_prompt(
            "  User: hello  ",
            NaiveDate::from_ymd_opt(2026, 10, 16).unwrap(),
        );
        assert_eq!(p, "TODAY: 2026-10-16\n\nINPUT:\nUser: hello");
        assert!(SYSTEM_PROMPT.contains("ONLY a JSON object"));
        // The examples must themselves be valid replies, or they teach the
        // model a shape this parser rejects.
        for example in SYSTEM_PROMPT.split("OUTPUT:\n").skip(1) {
            let json = example.lines().next().unwrap();
            assert!(!parse_facts(json).unwrap().is_empty(), "{json}");
        }
    }
}

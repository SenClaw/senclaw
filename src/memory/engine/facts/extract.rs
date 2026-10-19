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

use super::slots::{self, Slot};
use super::store::{entity_key, FactKind};

/// A fact longer than this is a paragraph the model failed to split.
pub const MAX_FACT_CHARS: usize = 600;
/// Shorter than this is a fragment ("Sen.").
pub const MIN_FACT_CHARS: usize = 8;
const MAX_FACTS_PER_CHUNK: usize = 24;
const MAX_ENTITIES_PER_FACT: usize = 8;
const MAX_ENTITY_CHARS: usize = 80;

pub const SYSTEM_PROMPT: &str = r#"You extract durable memories from a conversation or document, so an assistant can recall them months from now.

Reply with ONLY a JSON object, nothing before or after it:
{"facts":[{"text":"...","kind":"world","when":"2026-06-09","entities":["Emily","Sarah"],"subject":"","predicate":""}]}
Reply {"facts":[]} when nothing is worth keeping. "subject" and "predicate" are explained under SLOTS; leave them "" for most facts.

KNOWN FACTS: the input may be followed by a list of facts already stored, each tagged [F1], [F2]… Do not state a known fact again unless something about it changed. When a new fact CORRECTS or REPLACES a known one — the person changed their mind, a date or number moved, something stopped being true — add "replaces":["F2"] to the new fact. Replace only on a real contradiction about the same thing; a related or additional fact replaces nothing.

SLOTS (optional): when a fact states the ONE current value of an attribute — someone's name, where they live or work, their job, a price or status, or their favourite / preferred / current choice of something — add "subject" (who or what it is about; "user" for the user) and "predicate" (short, lowercase, English, snake_case: name, lives_in, works_at, job_title, age, email, phone, status, price, or favorite_<thing> / current_<thing> / preferred_<thing> for a choice, for example favorite_frontend_framework). A known fact may show its slot as <user · favorite_frontend_framework>: when the new fact gives a new value for that same attribute — the person moved, switched, changed their mind, a price or status changed — give it that SAME subject and EXACTLY that predicate; that alone is enough for the old value to be replaced. Leave subject and predicate "" for everything else — events, plans, relationships, things the user likes several of.

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

Example (TODAY: 2026-10-16) — a changed mind
INPUT:
User: tôi chuyển sang Vue rồi, không dùng React nữa

KNOWN FACTS:
[F1] <user · favorite_frontend_framework> Người dùng thích dùng React cho frontend.
[F2] <user · name> Người dùng tên là Sen.
OUTPUT:
{"facts":[{"text":"Người dùng đã chuyển sang Vue và không còn dùng React.","kind":"world","when":null,"entities":["user","Vue","React"],"subject":"user","predicate":"favorite_frontend_framework","replaces":["F1"]}]}

Example (TODAY: 2026-10-16) — a move
INPUT:
User: tôi chuyển vào Đà Nẵng rồi, không ở Huế nữa

KNOWN FACTS:
[F1] <user · lives_in> Người dùng sống ở Huế.
OUTPUT:
{"facts":[{"text":"Người dùng đã chuyển vào Đà Nẵng sống.","kind":"world","when":null,"entities":["user","Đà Nẵng","Huế"],"subject":"user","predicate":"lives_in"}]}

Example (TODAY: 2026-10-16)
INPUT:
User: tôi tên là Sen, đang làm dự án SemaClaw, deadline cuối tháng 8 năm 2026
Assistant: Đã ghi nhận.
OUTPUT:
{"facts":[{"text":"Người dùng tên là Sen.","kind":"world","when":null,"entities":["Sen","user"],"subject":"user","predicate":"name"},{"text":"Sen đang làm dự án SemaClaw, hạn chót là cuối tháng 8 năm 2026.","kind":"world","when":"2026-08","entities":["Sen","SemaClaw"]}]}"#;

/// The user message: the date that relative phrases resolve against, the
/// text, and — when there are any — the facts already stored that look related,
/// numbered `[F1]…` so the reply can name the ones it replaces.
pub fn user_prompt(text: &str, today: NaiveDate, known: &[String]) -> String {
    let mut out = format!(
        "TODAY: {}\n\nINPUT:\n{}",
        today.format("%Y-%m-%d"),
        text.trim()
    );
    if !known.is_empty() {
        out.push_str("\n\nKNOWN FACTS:");
        for (i, fact) in known.iter().enumerate() {
            out.push_str(&format!("\n[F{}] {}", i + 1, fact));
        }
    }
    out
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
    /// 1-based numbers of the known facts (`[F1]` → 1) this one replaces, as
    /// the model named them. Unchecked here: whether a number points at
    /// anything, and whether it is plausible, is for the caller.
    pub replaces: Vec<usize>,
    /// Which attribute of whom this states, when the model said so and it made
    /// sense (see [`slots`]).
    pub slot: Option<Slot>,
}

/// Parse a model reply into facts. An unparseable reply is an `Err`; a parsed
/// reply with no usable facts is `Ok(vec![])` — "nothing worth keeping" is a
/// legitimate answer, not a failure.
///
/// A reply whose array has some elements the model garbled and others that read
/// fine is neither: the readable facts are returned and the garbled elements are
/// counted in a warning (see [`salvage_elements`]). It is not a failed chunk —
/// `RetainReport::failed_chunks` counts replies that yielded nothing to read.
pub fn parse_facts(raw: &str) -> Result<Vec<Extracted>> {
    let (items, dropped) = facts_array(raw)?;
    if dropped > 0 {
        tracing::warn!(
            kept = items.len(),
            dropped,
            "[facts] extraction reply partly garbled; unreadable elements dropped"
        );
    }
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

/// The facts array of a reply, and how many of its elements were too garbled to
/// read (0 unless [`salvage_elements`] had to step in).
fn facts_array(raw: &str) -> Result<(Vec<Value>, usize)> {
    let body = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    // `{"facts":[...]}` is what was asked for, after whatever prose or fence
    // the model put in front of it.
    if let Some(start) = body.find('{') {
        let object = &body[start..];
        match first_json_value(object) {
            Some(Value::Object(mut o)) => {
                if let Some(Value::Array(a)) = o.remove("facts") {
                    return Ok((a, 0));
                }
            }
            Some(_) => {}
            // Neither the reply as it is nor with its closers made up reads as
            // JSON: one element may be garbled inside an otherwise fine array.
            None => {
                if let Some(salvaged) = facts_key_body(object).and_then(salvage_elements) {
                    return Ok(salvaged);
                }
            }
        }
    }
    // A bare array is accepted only when the reply *is* one (optionally
    // fenced). Looking for any `[` would find the empty list inside
    // `{"triplets":[]}` and read a reply in the wrong shape as "no facts".
    if let Some(inside) = body.strip_prefix('[') {
        match first_json_value(body) {
            Some(Value::Array(a)) => return Ok((a, 0)),
            Some(_) => {}
            None => {
                if let Some(salvaged) = salvage_elements(inside) {
                    return Ok(salvaged);
                }
            }
        }
    }
    Err(anyhow!(
        "fact JSON not found in reply: {}",
        crate::util::text::truncate_on_char_boundary(raw.trim(), 200)
    ))
}

/// The first complete JSON value at the start of `s`, ignoring whatever follows
/// it. A small model often loses count of its closing brackets: one `}` too
/// many after the value is harmless; one too few is made up (see
/// [`with_missing_closers`]). Slicing from the first `{` to the last `}` threw
/// away every fact of the window in either case.
fn first_json_value(s: &str) -> Option<Value> {
    if let Some(Ok(v)) = serde_json::Deserializer::from_str(s)
        .into_iter::<Value>()
        .next()
    {
        return Some(v);
    }
    serde_json::from_str(&with_missing_closers(s)?).ok()
}

/// `s` with the `]` / `}` it is missing, innermost first — at the end, or in
/// front of a closer that arrives while something inside it is still open (the
/// `]` of the facts array dropped before the object's `}`). `None` when that is
/// not a repair: nothing is missing, a closer matches nothing that is open, or
/// the reply stops inside a string. A reply cut short in the middle of a fact's
/// text would otherwise store a sentence that ends halfway.
fn with_missing_closers(s: &str) -> Option<String> {
    let mut fixed = String::with_capacity(s.len() + 4);
    let mut open: Vec<char> = Vec::new();
    let (mut in_string, mut escaped, mut repaired) = (false, false, false);
    for c in s.chars() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else {
            match c {
                '"' => in_string = true,
                '{' => open.push('}'),
                '[' => open.push(']'),
                '}' | ']' => {
                    let depth = open.iter().rposition(|&o| o == c)?;
                    while open.len() > depth + 1 {
                        fixed.extend(open.pop());
                        repaired = true;
                    }
                    open.pop();
                }
                _ => {}
            }
        }
        fixed.push(c);
    }
    if in_string {
        return None;
    }
    if !open.is_empty() {
        // A comma left dangling goes with the closers it was waiting for.
        fixed.truncate(fixed.trim_end().trim_end_matches(',').len());
        fixed.extend(open.into_iter().rev());
        repaired = true;
    }
    repaired.then_some(fixed)
}

/// What follows `{"facts":[` at the start of `s`, the text of the array's body.
/// Anchored at the start: a `"facts":[` that turns up later is more likely
/// inside a fact's own text.
fn facts_key_body(s: &str) -> Option<&str> {
    s.strip_prefix('{')?
        .trim_start()
        .strip_prefix("\"facts\"")?
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('[')
}

/// The elements of an array that parse on their own, and how many did not, when
/// the array as a whole does not parse. `body` is the text after the array's
/// `[`.
///
/// The failure this is for: the model repeats the start of a fact's key,
/// `{"text{"text":"Lan đang nghỉ phép…","kind":"world",…}`, and the reply is no
/// longer JSON. About 1.4% of the replies of a 2B model (2 of 140 calls); every
/// fact of the window used to go with it. The other elements are fine, so the
/// garbled one is dropped and the rest kept.
///
/// An element is read with a real JSON parser, never by matching braces: the
/// repeated key carries one quote too many, so a scan that tracks strings reads
/// everything after it inverted — braces inside strings count, the real ones do
/// not — and would take the elements behind the garbled one with it. When an
/// element does not parse, the next one is taken to start at the first
/// `},{"` that is not inside a `[…]` of the garbled element (a wrapped
/// `"entities":[{"text":"A"},{"text":"B"}]` is full of them). A raw quote cannot
/// occur inside a JSON string, so only a text that ends in `},{` could hide a
/// boundary — and then the neighbours are dropped, never invented.
///
/// `None` — leave the reply an error — unless at least one element was read and
/// at least one dropped, and unless the array was closed: a reply cut off in the
/// middle of an element is the length limit, not a glitch, and (as ever) is not
/// patched into half a sentence. Elements that parse but are not facts are for
/// [`read_fact`] to drop, as before; they do not count here.
fn salvage_elements(body: &str) -> Option<(Vec<Value>, usize)> {
    let mut items: Vec<Value> = Vec::new();
    let mut dropped = 0;
    let mut rest = body.trim_start();
    loop {
        match rest.chars().next()? {
            ']' => break,
            ',' => {
                rest = rest[1..].trim_start();
                continue;
            }
            _ => {}
        }
        let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        if let Some(Ok(value)) = stream.next() {
            items.push(value);
            rest = rest[stream.byte_offset()..].trim_start();
            continue;
        }
        dropped += 1;
        match next_element_start(rest) {
            Some(at) => rest = &rest[at..],
            // The last element: the array must have been closed after it.
            None if closes_array(rest) => break,
            None => return None,
        }
    }
    (!items.is_empty() && dropped > 0).then_some((items, dropped))
}

/// Where the element after the garbled one that starts `s` begins: the `{` of
/// the first `},{"` outside a nested `[…]`.
fn next_element_start(s: &str) -> Option<usize> {
    let mut nested = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '[' => nested += 1,
            ']' => nested = nested.saturating_sub(1),
            '}' if nested == 0 => {
                let after = &s[i + 1..];
                let Some(next) = after.trim_start().strip_prefix(',') else {
                    continue;
                };
                let next = next.trim_start();
                if next
                    .strip_prefix('{')
                    .is_some_and(|k| k.trim_start().starts_with('"'))
                {
                    return Some(s.len() - next.len());
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether the last, garbled element `s` is followed by the array's `]`: a `}`
/// then a `]`. Cut off inside the element it has neither.
fn closes_array(s: &str) -> bool {
    s.char_indices()
        .any(|(i, c)| c == '}' && s[i + 1..].trim_start().starts_with(']'))
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
        replaces: read_replaces(obj.get("replaces")),
        slot: read_slot(obj),
    })
}

/// `"subject"` and `"predicate"` beside the text, or the same two inside a
/// `"slot"` object — some models nest them. Either half missing or unusable
/// means no slot; the fact is kept all the same.
fn read_slot(obj: &serde_json::Map<String, Value>) -> Option<Slot> {
    let holder = obj.get("slot").and_then(Value::as_object).unwrap_or(obj);
    let text = |key: &str| holder.get(key).and_then(Value::as_str);
    slots::read(text("subject")?, text("predicate")?)
}

/// `["F2"]`, `["f2"]`, `[2]` and `"F2"` all mean fact 2; anything else is
/// ignored, and so is 0 (the numbering starts at 1).
fn read_replaces(v: Option<&Value>) -> Vec<usize> {
    let one = |v: &Value| -> Option<usize> {
        match v {
            Value::Number(n) => n.as_u64().map(|n| n as usize),
            Value::String(s) => s
                .trim()
                .trim_start_matches(['F', 'f', '#'])
                .parse::<usize>()
                .ok(),
            _ => None,
        }
        .filter(|n| *n >= 1)
    };
    let mut out: Vec<usize> = match v {
        Some(Value::Array(list)) => list.iter().filter_map(one).collect(),
        Some(single) => one(single).into_iter().collect(),
        None => Vec::new(),
    };
    out.sort_unstable();
    out.dedup();
    out
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
    fn a_slot_is_read_flat_or_nested_and_only_when_both_halves_make_sense() {
        let raw = r#"{"facts":[
            {"text":"Người dùng sống ở Huế.","subject":"Người dùng","predicate":"Lives in"},
            {"text":"Người dùng làm ở FPT.","slot":{"subject":"tôi","predicate":"employer"}},
            {"text":"Người dùng thích cà phê đen.","subject":"user"},
            {"text":"Người dùng thích trà xanh nóng.","subject":"","predicate":"likes"},
            {"text":"Người dùng uống nước lọc buổi sáng.","subject":"user","predicate":"???"},
            {"text":"Người dùng có một con mèo tên Mun.","subject":7,"predicate":["x"]}]}"#;
        let facts = parse_facts(raw).unwrap();
        let slot = |i: usize| {
            facts[i]
                .slot
                .as_ref()
                .map(|s| (s.subject.as_str(), s.predicate.as_str()))
        };
        assert_eq!(slot(0), Some(("user", "lives_in")));
        assert_eq!(
            slot(1),
            Some(("user", "works_at")),
            "nested, and an alias of works_at"
        );
        for i in 2..facts.len() {
            assert_eq!(slot(i), None, "{}", facts[i].text);
        }
        assert_eq!(facts.len(), 6, "a bad slot never costs the fact");
    }

    #[test]
    fn the_prompt_teaches_slots_and_shows_one_in_its_changed_mind_example() {
        assert!(SYSTEM_PROMPT.contains("SLOTS (optional)"));
        assert!(SYSTEM_PROMPT.contains("<user · favorite_frontend_framework>"));
        assert!(SYSTEM_PROMPT.contains(r#""predicate":"favorite_frontend_framework""#));
        // The move example names no fact to replace: the slot alone is the lesson.
        assert!(SYSTEM_PROMPT.contains(r#""predicate":"lives_in"}]}"#));
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

    // The three ways the 2B model (gemma-4-e2b, 4-bit) really lost count of its
    // brackets in the #[ignore] real-extractor eval, 4 of 133 replies.

    /// Three facts, then nothing: the closing `]}` never came.
    const MISSING_ARRAY_AND_OBJECT: &str = r#"{"facts":[{"text":"Người dùng sử dụng một khóa API cho dự án Orion và dự kiến triển khai vào ngày 2026-11-02.","kind":"world","when":"2026-11-02","entities":["user","Orion"]},{"text":"Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ và cấu hình lại phiên, mất khoảng hai tiếng.","kind":"world","when":null,"entities":["user"]},{"text":"Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm sang 4802 và khởi động lại daemon.","kind":"world","when":null,"entities":["Assistant"]}"#;
    /// The array closed, the object did not.
    const MISSING_OBJECT: &str = r#"{"facts":[{"text":"Người dùng đã chuyển sang sử dụng Vue và không còn dùng React cho phần frontend của dự án.","kind":"world","when":null,"entities":["user","Vue","React"]}]"#;
    /// Closed properly, then one `}` more.
    const EXTRA_BRACE: &str = r#"{"facts":[{"text":"Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm sang 4802 và khởi động lại daemon.","kind":"experience","when":"2026-10-16","entities":["assistant"]},{"text":"Người dùng đồng ý với hành động trên.","kind":"world","when":"2026-10-16","entities":["user"]},{"text":"Hôm qua, người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ và cấu hình lại phiên, mất khoảng hai tiếng.","kind":"world","when":"2026-10-15","entities":["user"]} ]}}"#;

    /// The array's `]` dropped: the object closed, then the reply's `}`.
    const MISSING_ARRAY: &str = r#"{"facts":[{"text":"Hôm qua, người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ và cấu hình lại phiên. Việc này mất khoảng hai tiếng.","kind":"world","when":"2026-10-15","entities":["user"]} }"#;

    #[test]
    fn a_reply_that_lost_count_of_its_brackets_still_yields_every_fact() {
        let facts = parse_facts(MISSING_ARRAY_AND_OBJECT).unwrap();
        assert_eq!(facts.len(), 3);
        assert_eq!(facts[0].occurred, Some(day(2026, 11, 2)));
        assert!(facts[2].text.ends_with("khởi động lại daemon."));

        let facts = parse_facts(MISSING_OBJECT).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].entities, vec!["user", "vue", "react"]);

        let facts = parse_facts(EXTRA_BRACE).unwrap();
        assert_eq!(facts.len(), 3);
        assert_eq!(facts[2].occurred, Some(day(2026, 10, 15)));

        let facts = parse_facts(MISSING_ARRAY).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].occurred, Some(day(2026, 10, 15)));
    }

    #[test]
    fn repaired_replies_are_read_the_same_behind_prose_and_inside_a_fence() {
        for body in [
            MISSING_ARRAY_AND_OBJECT,
            MISSING_OBJECT,
            MISSING_ARRAY,
            EXTRA_BRACE,
        ] {
            let plain = parse_facts(body).unwrap();
            for raw in [
                format!("Sure! Here you go:\n{body}"),
                format!("```json\n{body}\n```"),
            ] {
                assert_eq!(parse_facts(&raw).unwrap(), plain, "{raw}");
            }
        }
    }

    #[test]
    fn trailing_text_after_a_complete_reply_is_ignored() {
        let body =
            r#"{"facts":[{"text":"The user prefers dark mode everywhere.","entities":["user"]}]}"#;
        for tail in [
            "\nNote: I skipped the greeting {as asked}.",
            " }",
            " ]",
            "\n```",
        ] {
            let facts = parse_facts(&format!("{body}{tail}")).unwrap();
            assert_eq!(facts.len(), 1, "{tail:?}");
        }
        // The bare array too.
        let facts =
            parse_facts(r#"[{"text":"The user prefers dark mode everywhere."}] That is all."#)
                .unwrap();
        assert_eq!(facts.len(), 1);
    }

    #[test]
    fn a_bare_array_that_lost_its_closing_bracket_is_repaired() {
        let facts = parse_facts(
            r#"[{"text":"The user prefers dark mode everywhere."},{"text":"The user lives in Hanoi with Sen."},"#,
        )
        .unwrap();
        assert_eq!(
            facts.len(),
            2,
            "a trailing comma goes with the missing bracket"
        );
    }

    #[test]
    fn a_reply_cut_inside_a_fact_is_not_patched_into_half_a_sentence() {
        // Stopping inside a string means the text ends halfway; closing the
        // brackets around it would store that.
        for raw in [
            r#"{"facts":[{"text":"The user prefers dark mode everywhere.","kind":"world"},{"text":"The user lives in Han"#,
            r#"{"facts":[{"text":"The user prefers dark mode everywhere.","kind":"wor"#,
            r#"{"facts":[{"text":"The user prefers dark mode everywhere.","when":nu"#,
            r#"{"facts":[{"text":"The user prefers dark mode everywhere.","kind"#,
        ] {
            assert!(parse_facts(raw).is_err(), "{raw}");
        }
        // An escaped quote and a bracket inside the text do not fool the scan.
        let facts = parse_facts(
            r#"{"facts":[{"text":"The user said \"use [Vue}\" for the new frontend.","kind":"world"}"#,
        )
        .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0].text,
            r#"The user said "use [Vue}" for the new frontend."#
        );
    }

    #[test]
    fn brackets_that_do_not_nest_are_not_repaired() {
        // A closer with nothing open to close is a typo, not a missing bracket.
        assert!(
            parse_facts(r#"{"facts":[{"text":"The user prefers dark mode everywhere."}]]"#)
                .is_err()
        );
        assert!(parse_facts(r#"{"triplets":["#).is_err());
        assert!(
            parse_facts(r#"{"triplets":[]"#).is_err(),
            "the wrong shape is still wrong"
        );
    }

    // The one shape left after that, 2 of the next 140 real replies of the same
    // model: a fact starts its key twice, `{"text{"text":`. It is not JSON
    // whatever closers are added, and its extra quote turns every string after
    // it inside out for a scanner that tracks them.

    /// Four facts, then the garbled one, last.
    const GARBLED_LAST: &str = r#"{"facts":[{"text":"Lan phụ trách dự án Falcon.","kind":"world","when":null,"entities":["Lan","Falcon"]},{"text":"Lan sẽ nghỉ phép đến hết tháng 11.","kind":"world","when":"2026-10-16","entities":["Lan"]},{"text":"Người dùng đang làm việc liên quan đến ứng dụng thanh toán.","kind":"world","when":null,"entities":["user","ứng dụng thanh toán"]},{"text":"Emily đã chuyển đến Huế và bắt đầu làm việc tại FPT vào tháng 3 năm nay.","kind":"world","when":"2026-03","entities":["Emily","Huế","FPT"]},{"text{"text":"Hôm qua, người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ và cấu hình lại phiên, mất khoảng hai tiếng.","kind":"world","when":"2026-10-15","entities":["user","ứng dụng thanh toán"]}]}"#;
    /// Three facts (the first mentions a redacted key, which `read_fact` refuses),
    /// then two garbled ones in a row.
    const GARBLED_TWICE: &str = r#"{"facts":[{"text":"Khóa của người dùng là [REDACTED:api_key] và được sử dụng cho dự án Orion.","kind":"world","when":null,"entities":["user","Orion"]},{"text":"Orion dự kiến sẽ triển khai vào ngày 2026-11-02.","kind":"world","when":"2026-11-02","entities":["Orion"]},{"text":"Dự án Falcon do Lan phụ trách.","kind":"world","when":null,"entities":["Falcon","Lan"]},{"text{"text":"Lan đang nghỉ phép cho đến hết tháng 11.","kind":"world","when":null,"entities":["Lan"]},{"text{"text":"Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán hôm qua bằng cách xóa cookie cũ và cấu hình lại phiên, mất khoảng hai tiếng.","kind":"world","when":"2026-10-15","entities":["user","ứng dụng thanh toán"]}]}"#;

    const FACT_A: &str = r#"{"text":"The user prefers dark mode everywhere.","kind":"world","when":null,"entities":["user"]}"#;
    const FACT_B: &str = r#"{"text":"The user lives in Hanoi with Sen.","kind":"world","when":"2026-03","entities":["Sen","Hanoi"]}"#;
    /// The real garbled element of `GARBLED_LAST`, alone.
    fn garbled() -> &'static str {
        let from = GARBLED_LAST.find(r#"{"text{"#).unwrap();
        &GARBLED_LAST[from..GARBLED_LAST.len() - 2]
    }

    fn texts(raw: &str) -> Vec<String> {
        parse_facts(raw)
            .unwrap()
            .into_iter()
            .map(|f| f.text)
            .collect()
    }

    #[test]
    fn a_garbled_last_element_costs_only_itself() {
        assert!(
            serde_json::from_str::<Value>(GARBLED_LAST).is_err(),
            "the fixture is not JSON"
        );
        let (items, dropped) = facts_array(GARBLED_LAST).unwrap();
        assert_eq!((items.len(), dropped), (4, 1));

        let facts = parse_facts(GARBLED_LAST).unwrap();
        assert_eq!(facts.len(), 4);
        assert_eq!(facts[0].text, "Lan phụ trách dự án Falcon.");
        assert_eq!(facts[1].occurred, Some(day(2026, 10, 16)));
        assert_eq!(facts[3].entities, vec!["emily", "huế", "fpt"]);
        assert!(
            facts.iter().all(|f| !f.text.starts_with("Hôm qua")),
            "the garbled fact is dropped, not mended"
        );
    }

    #[test]
    fn two_garbled_elements_in_one_reply_leave_the_facts_before_them() {
        let (items, dropped) = facts_array(GARBLED_TWICE).unwrap();
        assert_eq!((items.len(), dropped), (3, 2));
        // The first of the three is a redacted key, which is out on its own account.
        let facts = parse_facts(GARBLED_TWICE).unwrap();
        assert_eq!(
            facts.iter().map(|f| f.text.as_str()).collect::<Vec<_>>(),
            [
                "Orion dự kiến sẽ triển khai vào ngày 2026-11-02.",
                "Dự án Falcon do Lan phụ trách."
            ]
        );
        assert_eq!(facts[0].occurred, Some(day(2026, 11, 2)));
    }

    #[test]
    fn the_facts_behind_a_garbled_element_survive_it() {
        // Wherever it sits, the real garbled element takes only itself. The
        // facts after it are what a scan that tracks strings would lose.
        let g = garbled();
        let a = "The user prefers dark mode everywhere.";
        let b = "The user lives in Hanoi with Sen.";
        for (raw, dropped) in [
            (format!(r#"{{"facts":[{FACT_A},{g},{FACT_B}]}}"#), 1),
            (format!(r#"{{"facts":[{g},{FACT_A},{FACT_B}]}}"#), 1),
            (format!(r#"{{"facts":[{FACT_A},{g},{g},{FACT_B}]}}"#), 2),
            (
                format!("{{\"facts\":[\n  {FACT_A},\n  {g},\n  {FACT_B}\n]}}"),
                1,
            ),
            (format!("[{FACT_A}, {g}, {FACT_B}]"), 1),
            (format!("[{g},{FACT_A},{FACT_B}]"), 1),
        ] {
            assert_eq!(facts_array(&raw).unwrap().1, dropped, "{raw}");
            assert_eq!(texts(&raw), [a, b], "{raw}");
        }
    }

    #[test]
    fn what_follows_a_garbled_element_is_read_as_it_was_written() {
        // Brackets, braces and an escaped quote inside a string, and a number
        // that is not a fact, behind the garbled element.
        let raw = format!(
            r#"{{"facts":[{},{{"text":"The user said \"use [Vue}} now\" for the new frontend.","kind":"world"}},7,{}]}}"#,
            garbled(),
            FACT_B
        );
        assert_eq!(
            texts(&raw),
            [
                r#"The user said "use [Vue} now" for the new frontend."#,
                "The user lives in Hanoi with Sen."
            ]
        );
    }

    #[test]
    fn a_garbled_element_with_wrapped_entities_does_not_start_a_fact_inside_itself() {
        // `},{"` occurs inside this element's own entities; the second of them
        // is long enough to pass as a fact if it were taken for an element.
        let wrapped = r#"{"text{"text":"Alice and Bob met at the Hà Nội office last year.","entities":[{"text":"Alice"},{"text":"Hà Nội office"}]}"#;
        let raw = format!(r#"{{"facts":[{FACT_A},{wrapped},{FACT_B}]}}"#);
        assert_eq!(
            texts(&raw),
            [
                "The user prefers dark mode everywhere.",
                "The user lives in Hanoi with Sen."
            ]
        );
    }

    #[test]
    fn salvaged_replies_are_read_the_same_behind_prose_and_inside_a_fence() {
        for body in [GARBLED_LAST, GARBLED_TWICE] {
            let plain = parse_facts(body).unwrap();
            for raw in [
                format!("Sure! Here you go:\n{body}"),
                format!("```json\n{body}\n```"),
                format!("{body}\nHope that helps."),
            ] {
                assert_eq!(parse_facts(&raw).unwrap(), plain, "{raw}");
            }
        }
    }

    #[test]
    fn a_reply_with_nothing_readable_in_it_is_still_a_failure() {
        // Every element garbled: nothing to keep is not "nothing worth keeping".
        for raw in [
            r#"{"facts":[{"text{"text":"Lan đang nghỉ phép cho đến hết tháng 11.","kind":"world"}]}"#.to_string(),
            format!(r#"{{"facts":[{0},{0}]}}"#, garbled()),
            format!("[{}]", garbled()),
            // Wrong shape stays wrong.
            r#"{"triplets":[{"a{"a":1},{"b":2}]}"#.to_string(),
        ] {
            assert!(parse_facts(&raw).is_err(), "{raw}");
        }
        // No element dropped means it was not this that went wrong.
        assert!(parse_facts(r#"{"facts":[] and then some"#).is_err());
    }

    #[test]
    fn a_garbled_reply_that_was_also_cut_off_is_not_salvaged() {
        let g = garbled();
        for raw in [
            // Cut inside the next fact.
            format!(r#"{{"facts":[{FACT_A},{g},{{"text":"The user lives in Han"#),
            format!(r#"{{"facts":[{FACT_A},{g},{FACT_B},{{"text":"The user lives in Han"#),
            // Cut inside the garbled one itself.
            format!(r#"{{"facts":[{FACT_A},{{"text{{"text":"The user lives in Han"#),
            // Cut right after an element, array unclosed.
            format!(r#"{{"facts":[{FACT_A},{g},"#),
            format!(r#"{{"facts":[{FACT_A},{g}"#),
        ] {
            assert!(parse_facts(&raw).is_err(), "{raw}");
        }
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
    fn known_facts_are_numbered_after_the_input() {
        let p = user_prompt(
            "User: đổi ý",
            NaiveDate::from_ymd_opt(2026, 10, 16).unwrap(),
            &[
                "Người dùng thích React.".into(),
                "Sen sống ở Hà Nội.".into(),
            ],
        );
        assert!(
            p.ends_with("\n\nKNOWN FACTS:\n[F1] Người dùng thích React.\n[F2] Sen sống ở Hà Nội."),
            "{p}"
        );
    }

    #[test]
    fn replaces_is_read_leniently_and_never_as_zero() {
        let raw = r##"{"facts":[
            {"text":"Người dùng đã chuyển sang Vue từ React.","replaces":["F1","f3",2,"#4","F1","x",0,null]},
            {"text":"Một fact khác không thay gì cả hết.","replaces":"F2"},
            {"text":"Một fact thứ ba cũng không thay gì cả.","replaces":{"a":1}},
            {"text":"Một fact thứ tư hoàn toàn không có trường đó."}]}"##;
        let f = parse_facts(raw).unwrap();
        assert_eq!(f[0].replaces, vec![1, 2, 3, 4]);
        assert_eq!(f[1].replaces, vec![2]);
        assert!(f[2].replaces.is_empty());
        assert!(f[3].replaces.is_empty());
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
            &[],
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

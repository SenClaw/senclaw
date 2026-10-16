//! What the agent can do with facts by itself: look through them and forget one.
//!
//! The MCP memory server runs in its own process with the database path and
//! nothing else — no embedding model, no daemon state — so lookup here is by
//! words and by date only, which needs neither. (Recall on the request path
//! has the vector arm as well; this is the deliberate, ask-for-it version.)
//!
//! Both tools are scoped to the agent's own folder: an agent cannot see or
//! delete another agent's facts.

use chrono::NaiveDate;

use super::extract::span_label;
use super::store::{FactStore, StoredFact};
use super::temporal::parse_window;
use crate::memory::cognitive::retrievers::build_fts_match;

pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 30;

/// Facts for a lookup: the newest ones when there is no query, otherwise the
/// best word matches, then — when the query names a time ("hôm qua", "tháng 3")
/// — whatever happened in that period, so "what did we do yesterday" works
/// with no word in common.
pub fn lookup(
    store: &FactStore,
    bank: &str,
    query: &str,
    today: NaiveDate,
    limit: usize,
) -> anyhow::Result<Vec<StoredFact>> {
    let limit = limit.clamp(1, MAX_LIMIT);
    let query = query.trim();
    if query.is_empty() {
        return store.recent(bank, limit, false);
    }
    let window = parse_window(query, today);
    let mut found = match build_fts_match(query) {
        Some(m) => store.search_keyword(bank, &m, window, limit)?,
        None => Vec::new(),
    };
    if let Some(window) = window {
        for f in store.in_window(bank, window, limit)? {
            if found.len() >= limit {
                break;
            }
            if !found.iter().any(|g| g.id == f.id) {
                found.push(f);
            }
        }
    }
    Ok(found)
}

/// One line per fact: `<id> · <when> · <text>`. The id is complete — it is what
/// `forget` takes, and a shortened one would be a guess.
pub fn render(facts: &[StoredFact]) -> String {
    if facts.is_empty() {
        return "No matching facts.".to_string();
    }
    facts
        .iter()
        .map(|f| {
            let when = match (f.occurred_start, f.occurred_end) {
                (Some(s), Some(e)) => span_label(s, e),
                (Some(s), None) => span_label(s, s + 86_400),
                _ => format!(
                    "noted {}",
                    chrono::DateTime::from_timestamp(f.mentioned_at, 0)
                        .map(|d| d.format("%Y-%m-%d").to_string())
                        .unwrap_or_default()
                ),
            };
            format!("{} · {} · {}", f.id, when, f.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Forget one of the agent's own facts by its full id.
pub fn forget(store: &FactStore, bank: &str, id: &str) -> String {
    match store.delete_in_bank(bank, id.trim()) {
        Ok(true) => format!("Forgot fact {}.", id.trim()),
        Ok(false) => format!(
            "No fact with id '{}' in this agent's memory. Use memory_search_facts to find the id.",
            id.trim()
        ),
        Err(e) => format!("Error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::memory::engine::facts::extract::parse_when;
    use crate::memory::engine::facts::store::{FactKind, Inserted, NewFact};
    use std::sync::Arc;

    fn store() -> FactStore {
        FactStore::new(Arc::new(Db::open_in_memory(&Config::from_env()).unwrap()))
    }

    fn add(s: &FactStore, bank: &str, text: &str, when: Option<&str>, said: i64) -> String {
        let f = NewFact {
            text: text.into(),
            kind: FactKind::World,
            occurred: when.and_then(parse_when),
            mentioned_at: said,
            entities: vec![],
            embedding: None,
            model: None,
            source: "test".into(),
        };
        match s.insert(bank, &f).unwrap() {
            Inserted::New(id) => id,
            other => panic!("{other:?}"),
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 16).unwrap()
    }

    fn texts(v: &[StoredFact]) -> Vec<&str> {
        v.iter().map(|f| f.text.as_str()).collect()
    }

    #[test]
    fn no_query_lists_the_newest_first() {
        let s = store();
        add(&s, "main", "older fact about Hue", None, 10);
        add(&s, "main", "newer fact about Hanoi", None, 20);
        let got = lookup(&s, "main", "  ", today(), 10).unwrap();
        assert_eq!(
            texts(&got),
            ["newer fact about Hanoi", "older fact about Hue"]
        );
    }

    #[test]
    fn a_query_finds_by_words() {
        let s = store();
        add(&s, "main", "Emily moved to Hue", None, 10);
        add(&s, "main", "The deploy is on Friday", None, 20);
        let got = lookup(&s, "main", "emily", today(), 10).unwrap();
        assert_eq!(texts(&got), ["Emily moved to Hue"]);
    }

    #[test]
    fn a_time_in_the_query_finds_facts_that_share_no_word_with_it() {
        let s = store();
        add(&s, "main", "Fixed the login bug", Some("2026-10-15"), 10);
        add(
            &s,
            "main",
            "Shipped the billing service",
            Some("2026-03-02"),
            11,
        );
        let got = lookup(&s, "main", "hôm qua", today(), 10).unwrap();
        assert_eq!(texts(&got), ["Fixed the login bug"]);
    }

    #[test]
    fn words_and_a_time_together_are_both_used_without_repeats() {
        let s = store();
        add(&s, "main", "Fixed the login bug", Some("2026-10-15"), 10);
        add(
            &s,
            "main",
            "Reviewed the login page",
            Some("2026-10-15"),
            11,
        );
        let got = lookup(&s, "main", "login hôm qua", today(), 10).unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn lookup_never_crosses_agents_and_clamps_the_limit() {
        let s = store();
        add(&s, "main", "mine", None, 1);
        add(&s, "other", "not mine", None, 2);
        assert_eq!(
            texts(&lookup(&s, "main", "", today(), 10).unwrap()),
            ["mine"]
        );
        for i in 0..40 {
            add(&s, "main", &format!("fact number {i}"), None, 100 + i);
        }
        assert_eq!(
            lookup(&s, "main", "", today(), 1000).unwrap().len(),
            MAX_LIMIT
        );
        assert_eq!(lookup(&s, "main", "", today(), 0).unwrap().len(), 1);
    }

    #[test]
    fn rendering_gives_the_whole_id_the_date_at_its_precision_and_the_text() {
        let s = store();
        let id = add(&s, "main", "Emily moved to Hue", Some("2026-03"), 10);
        let undated = add(&s, "main", "Sen likes tea", None, 1_792_152_000);
        let out = render(&lookup(&s, "main", "", today(), 10).unwrap());
        assert!(
            out.contains(&format!("{id} · 2026-03 · Emily moved to Hue")),
            "{out}"
        );
        assert!(
            out.contains(&format!("{undated} · noted 2026-10-16 · Sen likes tea")),
            "{out}"
        );
        assert_eq!(render(&[]), "No matching facts.");
    }

    #[test]
    fn forgetting_names_what_happened_and_is_scoped_to_the_agent() {
        let s = store();
        let mine = add(&s, "main", "wrong fact", None, 1);
        let theirs = add(&s, "other", "their fact", None, 2);

        assert_eq!(
            forget(&s, "main", &format!("  {mine} ")),
            format!("Forgot fact {mine}.")
        );
        assert_eq!(s.count("main").unwrap(), 0);

        let refused = forget(&s, "main", &theirs);
        assert!(refused.starts_with("No fact with id"), "{refused}");
        assert_eq!(
            s.count("other").unwrap(),
            1,
            "another agent's fact is untouched"
        );
        assert!(forget(&s, "main", "nope").contains("memory_search_facts"));
    }
}

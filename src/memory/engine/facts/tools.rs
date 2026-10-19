//! What the agent can do with facts by itself: look through them and forget one.
//!
//! The MCP memory server runs in its own process with the database path and
//! nothing else — no embedding model, no daemon state — so lookup here is by
//! words and by date only, which needs neither. (Recall on the request path
//! has the vector arm as well; this is the deliberate, ask-for-it version.)
//!
//! Both tools are scoped to the agent's own folder: an agent cannot see or
//! delete another agent's facts. A chat in a project also reaches the project's
//! bank, which its server learns from the environment, never from a parameter.

use chrono::NaiveDate;

use super::extract::span_label;
use super::store::{FactStore, Scope, StoredFact};
use super::temporal::parse_window;
use crate::memory::cognitive::retrievers::build_fts_match;

pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 30;

/// Facts for a lookup: the newest ones when there is no query, otherwise the
/// best word matches, then — when the query names a time ("hôm qua", "tháng 3")
/// — whatever happened in that period, so "what did we do yesterday" works
/// with no word in common. `scope` is what the asking conversation may see.
pub fn lookup(
    store: &FactStore,
    bank: &str,
    query: &str,
    today: NaiveDate,
    limit: usize,
    scope: Scope<'_>,
) -> anyhow::Result<Vec<StoredFact>> {
    let limit = limit.clamp(1, MAX_LIMIT);
    let query = query.trim();
    if query.is_empty() {
        return store.recent(bank, limit, false, scope);
    }
    let window = parse_window(query, today);
    let mut found = match build_fts_match(query) {
        Some(m) => store.search_keyword(bank, &m, window, limit, scope)?,
        None => Vec::new(),
    };
    if let Some(window) = window {
        for f in store.in_window(bank, window, limit, scope)? {
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

/// The project a chat belongs to, as the fact tools see it.
#[derive(Debug, Clone, Copy)]
pub struct ProjectView<'a> {
    /// The project's fact bank (`project:<id>`).
    pub bank: &'a str,
    /// Whether the agent's own facts are still shown beside the project's.
    pub include_common: bool,
}

/// [`lookup`] for a chat that may be in a project: the project's facts first —
/// the chat is working on them — then the agent's, unless the project hides
/// them, up to `limit` in all. Every fact of the project is visible to every
/// chat in it.
pub fn lookup_for_chat(
    store: &FactStore,
    bank: &str,
    project: Option<ProjectView<'_>>,
    query: &str,
    today: NaiveDate,
    limit: usize,
    scope: Scope<'_>,
) -> anyhow::Result<Vec<StoredFact>> {
    let Some(project) = project else {
        return lookup(store, bank, query, today, limit, scope);
    };
    let limit = limit.clamp(1, MAX_LIMIT);
    let mut found = lookup(store, project.bank, query, today, limit, Scope::Agent)?;
    if project.include_common {
        for f in lookup(store, bank, query, today, limit, scope)? {
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

/// Forget one of the agent's own facts by its full id — one `scope` lets the
/// caller see, so a chat cannot delete a fact it was never shown.
pub fn forget(store: &FactStore, bank: &str, id: &str, scope: Scope<'_>) -> String {
    let id = id.trim();
    let visible = store
        .get(id)
        .ok()
        .flatten()
        .is_some_and(|f| scope.allows(bank, &f.origin));
    let outcome = if visible {
        store.delete_in_bank(bank, id)
    } else {
        Ok(false)
    };
    match outcome {
        Ok(true) => format!("Forgot fact {}.", id.trim()),
        Ok(false) => format!(
            "No fact with id '{}' in this agent's memory. Use memory_search_facts to find the id.",
            id.trim()
        ),
        Err(e) => format!("Error: {e}"),
    }
}

/// [`forget`] for a chat that may be in a project: the agent's own fact first,
/// then — when that is not the answer — the project's.
pub fn forget_for_chat(
    store: &FactStore,
    bank: &str,
    project_bank: Option<&str>,
    id: &str,
    scope: Scope<'_>,
) -> String {
    let own = forget(store, bank, id, scope);
    match project_bank {
        Some(pb) if own.starts_with("No fact with id") => forget(store, pb, id, Scope::Agent),
        _ => own,
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
            origin: String::new(),
            slot: None,
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

    fn add_in(s: &FactStore, bank: &str, origin: &str, text: &str) -> String {
        let f = NewFact {
            text: text.into(),
            kind: FactKind::World,
            occurred: None,
            mentioned_at: 10,
            entities: vec![],
            embedding: None,
            model: None,
            source: "test".into(),
            origin: origin.into(),
            slot: None,
        };
        match s.insert(bank, &f).unwrap() {
            Inserted::New(id) => id,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_chat_can_neither_see_nor_forget_what_another_chat_said() {
        let s = store();
        let mine = add_in(&s, "main", "web:a", "Sen likes tea");
        let theirs = add_in(&s, "main", "web:b", "Sen likes coffee");
        let shared = add_in(&s, "main", "", "Sen lives in Hue");
        let a = Scope::Chat("web:a");

        let listed = lookup(&s, "main", "", today(), 10, a).unwrap();
        // Same second, so the order between them is not defined: compare as a set.
        let mut listed = texts(&listed);
        listed.sort_unstable();
        assert_eq!(listed, ["Sen likes tea", "Sen lives in Hue"]);
        let searched = lookup(&s, "main", "sen", today(), 10, a).unwrap();
        assert_eq!(texts(&searched).len(), 2);
        assert!(!texts(&searched).contains(&"Sen likes coffee"));

        // A guessed or leaked id gets the same answer as one that does not exist.
        let refused = forget(&s, "main", &theirs, a);
        assert!(refused.starts_with("No fact with id"), "{refused}");
        assert_eq!(s.count("main").unwrap(), 3);
        assert!(forget(&s, "main", &mine, a).starts_with("Forgot"));
        assert!(forget(&s, "main", &shared, a).starts_with("Forgot"));
        assert_eq!(
            s.count("main").unwrap(),
            1,
            "only the other chat's fact is left"
        );
        // The owner's unscoped view still reaches it.
        assert!(forget(&s, "main", &theirs, Scope::Agent).starts_with("Forgot"));
    }

    #[test]
    fn no_query_lists_the_newest_first() {
        let s = store();
        add(&s, "main", "older fact about Hue", None, 10);
        add(&s, "main", "newer fact about Hanoi", None, 20);
        let got = lookup(&s, "main", "  ", today(), 10, Scope::Agent).unwrap();
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
        let got = lookup(&s, "main", "emily", today(), 10, Scope::Agent).unwrap();
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
        let got = lookup(&s, "main", "hôm qua", today(), 10, Scope::Agent).unwrap();
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
        let got = lookup(&s, "main", "login hôm qua", today(), 10, Scope::Agent).unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn lookup_never_crosses_agents_and_clamps_the_limit() {
        let s = store();
        add(&s, "main", "mine", None, 1);
        add(&s, "other", "not mine", None, 2);
        assert_eq!(
            texts(&lookup(&s, "main", "", today(), 10, Scope::Agent).unwrap()),
            ["mine"]
        );
        for i in 0..40 {
            add(&s, "main", &format!("fact number {i}"), None, 100 + i);
        }
        assert_eq!(
            lookup(&s, "main", "", today(), 1000, Scope::Agent)
                .unwrap()
                .len(),
            MAX_LIMIT
        );
        assert_eq!(
            lookup(&s, "main", "", today(), 0, Scope::Agent)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn rendering_gives_the_whole_id_the_date_at_its_precision_and_the_text() {
        let s = store();
        let id = add(&s, "main", "Emily moved to Hue", Some("2026-03"), 10);
        let undated = add(&s, "main", "Sen likes tea", None, 1_792_152_000);
        let out = render(&lookup(&s, "main", "", today(), 10, Scope::Agent).unwrap());
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
            forget(&s, "main", &format!("  {mine} "), Scope::Agent),
            format!("Forgot fact {mine}.")
        );
        assert_eq!(s.count("main").unwrap(), 0);

        let refused = forget(&s, "main", &theirs, Scope::Agent);
        assert!(refused.starts_with("No fact with id"), "{refused}");
        assert_eq!(
            s.count("other").unwrap(),
            1,
            "another agent's fact is untouched"
        );
        assert!(forget(&s, "main", "nope", Scope::Agent).contains("memory_search_facts"));
    }

    const PROJECT: &str = "project:prj_0a1b2c3d";

    fn view(include_common: bool) -> Option<ProjectView<'static>> {
        Some(ProjectView {
            bank: PROJECT,
            include_common,
        })
    }

    #[test]
    fn a_chat_in_a_project_finds_the_projects_facts_first_and_then_the_agents() {
        let s = store();
        add_in(&s, PROJECT, "", "The deploy is on Friday");
        add_in(&s, "main", "", "Sen will deploy from the laptop");
        add_in(&s, "main", "web:a", "Sen will deploy the blog on Monday");
        add_in(&s, "main", "web:b", "Sen will deploy the wiki on Tuesday");
        let a = Scope::Chat("web:a");

        let got = lookup_for_chat(&s, "main", view(true), "deploy", today(), 10, a).unwrap();
        assert_eq!(
            texts(&got)[0],
            "The deploy is on Friday",
            "the project's facts come first"
        );
        // The agent's facts follow, through this chat's scope: so not web:b's.
        let mut rest = texts(&got)[1..].to_vec();
        rest.sort_unstable();
        assert_eq!(
            rest,
            [
                "Sen will deploy from the laptop",
                "Sen will deploy the blog on Monday"
            ]
        );
        // The same question from outside any project never sees the project.
        let outside = lookup_for_chat(&s, "main", None, "deploy", today(), 10, a).unwrap();
        assert!(!texts(&outside).contains(&"The deploy is on Friday"));
        // Inheriting nothing leaves only the project's own.
        let own = lookup_for_chat(&s, "main", view(false), "deploy", today(), 10, a).unwrap();
        assert_eq!(texts(&own), ["The deploy is on Friday"]);
        // A listing, no query, is the same union.
        let all = lookup_for_chat(&s, "main", view(true), "", today(), 10, a).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn the_project_cannot_crowd_out_the_limit_and_nothing_repeats() {
        let s = store();
        for i in 0..5 {
            add_in(&s, PROJECT, "", &format!("project fact number {i}"));
        }
        add_in(&s, "main", "", "agent fact");
        let got = lookup_for_chat(&s, "main", view(true), "", today(), 3, Scope::Agent).unwrap();
        assert_eq!(got.len(), 3, "the limit holds across both banks");
        let mut ids: Vec<_> = got.iter().map(|f| f.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn a_chat_in_a_project_can_forget_the_projects_facts_but_not_other_projects() {
        let s = store();
        let mine = add_in(&s, PROJECT, "", "The deploy is on Friday");
        let other = add_in(&s, "project:prj_ffffffff", "", "Another project's fact");
        let a = Scope::Chat("web:a");

        let refused = forget_for_chat(&s, "main", Some(PROJECT), &other, a);
        assert!(refused.starts_with("No fact with id"), "{refused}");
        assert_eq!(s.count("project:prj_ffffffff").unwrap(), 1);
        // Outside a project the project's fact is out of reach.
        let outside = forget_for_chat(&s, "main", None, &mine, a);
        assert!(outside.starts_with("No fact with id"), "{outside}");
        assert_eq!(s.count(PROJECT).unwrap(), 1);

        assert!(forget_for_chat(&s, "main", Some(PROJECT), &mine, a).starts_with("Forgot"));
        assert_eq!(s.count(PROJECT).unwrap(), 0);
    }
}

//! A fact's slot: which attribute of whom it states.
//!
//! "The user's favourite frontend framework is React" and, later, "…is Vue"
//! state two values of one attribute. Only one of them is true now, and the
//! store should know that without anyone naming the older fact — a small model
//! asked to point at "F2" mostly does not (measured: 0 of 2 changed minds on the
//! real chat path, against 15 of 18 on scripted windows).
//!
//! So the extractor is asked, *optionally*, for a `subject` and a `predicate`.
//! They are normalised here into a [`Slot`]; a new fact whose predicate holds a
//! single value at a time replaces the current fact in the same slot by rule.
//! A predicate that is not known to be single-valued is treated as many-valued:
//! keeping a stale fact can be undone by looking, hiding a true one is silent.
//! The single-valued seed comes from the knowledge graph's predicate table
//! (`cog_predicate_meta`), which this takes over from.

use super::store::entity_key;

/// Which attribute of whom: `("user", "lives_in")`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Slot {
    /// An [`entity_key`]; "user" for the person talking.
    pub subject: String,
    /// Short lowercase `snake_case`, in English, after [`canonical_predicate`].
    pub predicate: String,
}

/// Ways the model may spell "the person talking".
const USER_SUBJECTS: &[&str] = &[
    "user",
    "the user",
    "người dùng",
    "nguoi dung",
    "tôi",
    "mình",
    "i",
    "me",
    "speaker",
    "người nói",
];

const MAX_PREDICATE_CHARS: usize = 48;
const MAX_SUBJECT_CHARS: usize = 80;

/// Predicates that hold one value at a time.
const SINGLE_VALUED: &[&str] = &[
    // identity and attributes
    "name",
    "full_name",
    "age",
    "birthday",
    "born_in",
    "lives_in",
    "located_in",
    "address",
    "email",
    "phone",
    "works_at",
    "job_title",
    "role",
    "owner",
    "capital_of",
    // state
    "status",
    "state",
    "version",
    "current_version",
    "has_status",
    // measurements and readings
    "price",
    "sell_price",
    "buy_price",
    "cost",
    "exchange_rate",
    "rate",
    "temperature",
];

/// A choice is one value at a time when the name says so: `favorite_color`,
/// `current_laptop`, `preferred_editor`.
const SINGLE_VALUED_PREFIXES: &[&str] = &[
    "favorite_",
    "favourite_",
    "preferred_",
    "current_",
    "primary_",
    "main_",
    "default_",
];

/// Spellings a model uses for the same attribute.
const ALIASES: &[(&str, &str)] = &[
    ("resides_in", "lives_in"),
    ("lives_at", "lives_in"),
    ("residence", "lives_in"),
    ("home", "lives_in"),
    ("city", "lives_in"),
    ("location", "lives_in"),
    ("employer", "works_at"),
    ("works_for", "works_at"),
    ("company", "works_at"),
    ("workplace", "works_at"),
    ("occupation", "job_title"),
    ("profession", "job_title"),
    ("position", "job_title"),
    ("title", "job_title"),
    ("job", "job_title"),
    ("called", "name"),
    ("named", "name"),
    ("phone_number", "phone"),
    ("mobile", "phone"),
    ("email_address", "email"),
    ("owned_by", "owner"),
    ("has_status", "status"),
];

/// `Live in`, `lives-in` and `LIVES_IN` are one predicate; so are the
/// spellings in [`ALIASES`]. Empty when nothing usable is left.
pub fn canonical_predicate(raw: &str) -> String {
    let mut p: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    while p.contains("__") {
        p = p.replace("__", "_");
    }
    let p = p.trim_matches('_');
    let p: String = p.chars().take(MAX_PREDICATE_CHARS).collect();
    // `favourite_x` → `favorite_x`: the one alias that is a prefix.
    let p = match p.strip_prefix("favourite_") {
        Some(rest) => format!("favorite_{rest}"),
        None => p,
    };
    ALIASES
        .iter()
        .find(|(from, _)| *from == p)
        .map(|(_, to)| (*to).to_string())
        .unwrap_or(p)
}

/// The subject as the key facts are joined on, with every way of saying "the
/// user" folded into `user`.
pub fn canonical_subject(raw: &str) -> String {
    let key = entity_key(raw);
    if USER_SUBJECTS.contains(&key.as_str()) {
        "user".to_string()
    } else {
        key
    }
}

/// A slot from what the model wrote, or `None` when either half is missing,
/// empty or absurd.
pub fn read(subject: &str, predicate: &str) -> Option<Slot> {
    let subject = canonical_subject(subject);
    let predicate = canonical_predicate(predicate);
    if subject.is_empty()
        || predicate.is_empty()
        || subject.chars().count() > MAX_SUBJECT_CHARS
        || predicate.chars().all(|c| c == '_' || c.is_ascii_digit())
    {
        return None;
    }
    Some(Slot { subject, predicate })
}

/// Does a new value of this predicate replace the old one?
pub fn is_single_valued(predicate: &str) -> bool {
    if SINGLE_VALUED.contains(&predicate) {
        return true;
    }
    // `favorite_frameworks` names several; `favorite_framework` names one.
    SINGLE_VALUED_PREFIXES
        .iter()
        .any(|prefix| predicate.starts_with(prefix) && predicate.len() > prefix.len())
        && !predicate.ends_with('s')
}

impl Slot {
    /// `user · lives_in`, as the extractor is shown it beside a known fact so it
    /// can reuse the predicate when it states a new value.
    pub fn label(&self) -> String {
        format!("{} · {}", self.subject, self.predicate)
    }

    pub fn single_valued(&self) -> bool {
        is_single_valued(&self.predicate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_way_of_saying_the_user_is_the_user() {
        for s in ["user", "User", "the user", "Người dùng", "tôi", "  Mình "] {
            assert_eq!(canonical_subject(s), "user", "{s:?}");
        }
        assert_eq!(canonical_subject("Emily"), "emily");
        assert_eq!(
            canonical_subject("Hà  Nội"),
            "hà nội",
            "other subjects keep their spelling"
        );
    }

    #[test]
    fn a_predicate_is_one_spelling_however_it_was_written() {
        assert_eq!(canonical_predicate("Lives in"), "lives_in");
        assert_eq!(canonical_predicate("lives-in"), "lives_in");
        assert_eq!(canonical_predicate("  LIVES__IN "), "lives_in");
        assert_eq!(canonical_predicate("resides_in"), "lives_in");
        assert_eq!(canonical_predicate("employer"), "works_at");
        assert_eq!(canonical_predicate("Favourite Color"), "favorite_color");
        assert_eq!(canonical_predicate("???"), "");
    }

    #[test]
    fn a_slot_needs_both_halves() {
        assert_eq!(
            read("Người dùng", "Lives in"),
            Some(Slot {
                subject: "user".into(),
                predicate: "lives_in".into()
            })
        );
        assert_eq!(read("", "lives_in"), None);
        assert_eq!(read("user", ""), None);
        assert_eq!(read("user", "!!!"), None);
        assert_eq!(read("user", "2026"), None, "a number is not an attribute");
        assert_eq!(read(&"x".repeat(200), "lives_in"), None);
    }

    #[test]
    fn attributes_with_one_value_replace_and_the_rest_do_not() {
        for single in [
            "lives_in",
            "works_at",
            "name",
            "price",
            "favorite_frontend_framework",
            "current_laptop",
            "preferred_editor",
        ] {
            assert!(is_single_valued(single), "{single}");
        }
        for many in [
            "likes",
            "knows",
            "uses",
            "is_a",
            "includes",
            "prefers",
            "favorite_",      // a bare prefix says nothing
            "favorite_foods", // plural: several
            "visited",
        ] {
            assert!(!is_single_valued(many), "{many}");
        }
    }

    #[test]
    fn the_label_is_what_the_extractor_is_shown() {
        let s = read("user", "favorite frontend framework").unwrap();
        assert_eq!(s.label(), "user · favorite_frontend_framework");
        assert!(s.single_valued());
    }
}

//! How many objects a predicate may hold at once — the rule that decides
//! whether a new fact **supersedes** the old one or merely joins it.
//!
//! Measured motivation (docs/temporal-graph-research.md): on a real graph,
//! `sell_price` held three simultaneous "current" values for the same shop
//! four days apart, while `has_task` legitimately held three. Telling those
//! two apart is the whole job, and it is a property of the *predicate*, not
//! of any individual fact — so it is answered by a lookup table, not by an
//! LLM call per triplet (21k edges in two weeks; that bill never ends).
//!
//! Storage is `cog_predicate_meta`, seeded in [`super::schema`]. Anything not
//! in the table is [`Cardinality::Multi`]: keeping a stale fact costs a
//! down-ranked row, silently deleting a valid one costs knowledge.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    /// One object per subject at a time — a new object supersedes the old.
    Single,
    /// Many objects are legitimate — nothing is superseded.
    Multi,
}

impl Cardinality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Multi => "multi",
        }
    }

    /// Parse the stored string. Unknown spellings read as `Multi` — the
    /// conservative direction, and the same default as a missing row, so a
    /// typo in the table degrades to "never supersede" rather than to
    /// "supersede everything".
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("single") {
            Self::Single
        } else {
            Self::Multi
        }
    }

    pub fn supersedes(self) -> bool {
        matches!(self, Self::Single)
    }
}

impl fmt::Display for Cardinality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Normalise a predicate for lookup. Extraction is an LLM writing free-form
/// snake_case, so `Sell_Price`, `sell price` and `sell_price` all arrive for
/// the same idea; without folding, the single-cardinality rule would miss in
/// silence and the graph would go back to stacking prices.
pub fn normalize(predicate: &str) -> String {
    let mut out = String::with_capacity(predicate.len());
    let mut prev_sep = false;
    for ch in predicate.trim().chars() {
        if ch.is_alphanumeric() {
            for c in ch.to_lowercase() {
                out.push(c);
            }
            prev_sep = false;
        } else if !out.is_empty() && !prev_sep {
            out.push('_');
            prev_sep = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_predicates_never_supersede() {
        assert_eq!(Cardinality::parse("wat"), Cardinality::Multi);
        assert!(!Cardinality::parse("").supersedes());
        assert!(Cardinality::parse("single").supersedes());
        assert!(Cardinality::parse("SINGLE").supersedes());
    }

    #[test]
    fn normalize_folds_the_shapes_an_llm_emits() {
        assert_eq!(normalize("sell_price"), "sell_price");
        assert_eq!(normalize("Sell Price"), "sell_price");
        assert_eq!(normalize("  SELL-PRICE  "), "sell_price");
        assert_eq!(normalize("has__status"), "has_status");
        assert_eq!(normalize("MENTIONS"), "mentions");
    }

    #[test]
    fn normalize_keeps_non_ascii_predicates_usable() {
        // Extraction is told to emit English predicates, but small models
        // slip; folding must not produce an empty key.
        assert_eq!(normalize("giá bán"), "giá_bán");
        assert!(!normalize("giá bán").is_empty());
    }
}

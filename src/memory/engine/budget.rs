//! Packing results into a token budget.
//!
//! An agent's context is counted in tokens, not results. Items are taken best
//! first until the budget is spent; one too long for what is left is skipped
//! and the next is tried, so a single long note does not cost the shorter ones
//! behind it. If nothing fits at all the top result is still returned — a query
//! that matched something is never answered with nothing.

use crate::memory::chunker::estimate_tokens;

use super::types::Item;

/// Token cost of showing an item (header and body; the framing is negligible
/// next to them and constant per item).
pub fn cost(item: &Item) -> usize {
    estimate_tokens(&item.hit.header) + estimate_tokens(&item.hit.body)
}

/// Fill `max_tokens` from a best-first list. Sets each kept item's `tokens`.
/// Returns the kept items and how many were left out.
pub fn pack(items: Vec<Item>, max_tokens: usize) -> (Vec<Item>, usize) {
    let total = items.len();
    let mut kept: Vec<Item> = Vec::new();
    let mut used = 0usize;
    let mut first: Option<Item> = None;
    for mut item in items {
        item.tokens = cost(&item);
        if first.is_none() {
            first = Some(item.clone());
        }
        if used + item.tokens <= max_tokens {
            used += item.tokens;
            kept.push(item);
        }
    }
    if kept.is_empty() {
        kept.extend(first);
    }
    let left_out = total - kept.len();
    (kept, left_out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::engine::types::Hit;

    fn item(key: &str, words: usize) -> Item {
        Item {
            hit: Hit {
                arm: "curated",
                key: key.into(),
                header: String::new(),
                body: vec!["word"; words].join(" "),
                at: None,
                proof: 1,
            },
            score: 0.0,
            sources: vec![],
            tokens: 0,
        }
    }

    #[test]
    fn takes_best_first_until_the_budget_is_spent() {
        let (kept, left_out) = pack(vec![item("a", 40), item("b", 40), item("c", 40)], 100);
        assert_eq!(
            kept.iter().map(|i| i.hit.key.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(left_out, 1);
        assert_eq!(kept[0].tokens, 40);
    }

    #[test]
    fn a_long_item_is_skipped_and_shorter_ones_behind_it_still_fit() {
        let (kept, left_out) = pack(vec![item("a", 30), item("big", 200), item("c", 30)], 100);
        assert_eq!(
            kept.iter().map(|i| i.hit.key.as_str()).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(left_out, 1);
    }

    #[test]
    fn nothing_fitting_still_returns_the_best_result() {
        let (kept, left_out) = pack(vec![item("a", 500), item("b", 600)], 100);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].hit.key, "a");
        assert_eq!(left_out, 1);
    }

    #[test]
    fn an_empty_list_packs_to_nothing() {
        let (kept, left_out) = pack(vec![], 100);
        assert!(kept.is_empty());
        assert_eq!(left_out, 0);
    }
}

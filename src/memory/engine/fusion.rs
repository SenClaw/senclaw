//! Reciprocal rank fusion, and the merge of near-identical results.
//!
//! Scores from different stores are not comparable (a BM25 rank, a spreading
//! activation, a cosine), so fusion looks only at position:
//! `score(d) = Σ 1 / (k + rank_i(d))` over every list `d` appears in. An item
//! two strategies both rank well beats one a single strategy ranks first.

use std::collections::{HashMap, HashSet};

use super::types::Hit;

/// Smoothing constant of the original RRF paper; keeps one list's first place
/// from outweighing agreement between lists.
pub const RRF_K: f64 = 60.0;

/// Overlap at or above which two results count as the same thing said twice.
pub const DEFAULT_DUPLICATE_OVERLAP: f64 = 0.85;

/// Fewer distinct terms than this and an overlap says nothing: a three-word
/// line is "contained" in half the notes there are.
const MIN_TERMS_FOR_OVERLAP: usize = 6;

#[derive(Debug, Clone)]
pub struct Fused {
    pub hit: Hit,
    pub rrf: f64,
    pub sources: Vec<(&'static str, usize)>,
}

/// Fuse ranked lists, one per retrieval strategy, best first.
///
/// An item is identified by its store and key — `(hit.arm, hit.key)` — across
/// *all* lists, so the same fact found by two strategies (a vector search and a
/// keyword search over one store) is one result whose score is the sum of its
/// reciprocal ranks. That sum is the consensus the formula exists to reward.
/// Within one list a repeat counts once, at its first position.
///
/// `sources` records `(strategy, rank)` for every list an item appeared in.
/// The result is sorted by fused score; ties keep the earlier list and the
/// better rank, so the order is deterministic.
pub fn reciprocal_rank_fusion(lists: &[(&'static str, Vec<Hit>)]) -> Vec<Fused> {
    let mut out: Vec<Fused> = Vec::new();
    let mut index: HashMap<(&'static str, String), usize> = HashMap::new();
    for (strategy, list) in lists {
        let mut seen_here: HashSet<(&'static str, &str)> = HashSet::new();
        for (i, hit) in list.iter().enumerate() {
            if !seen_here.insert((hit.arm, hit.key.as_str())) {
                continue;
            }
            let rank = i + 1;
            let gain = 1.0 / (RRF_K + rank as f64);
            match index.get(&(hit.arm, hit.key.clone())) {
                Some(&at) => {
                    out[at].rrf += gain;
                    out[at].sources.push((strategy, rank));
                }
                None => {
                    index.insert((hit.arm, hit.key.clone()), out.len());
                    out.push(Fused {
                        hit: hit.clone(),
                        rrf: gain,
                        sources: vec![(strategy, rank)],
                    });
                }
            }
        }
    }
    // `sort_by` is stable, so equal scores keep list order.
    out.sort_by(|a, b| b.rrf.total_cmp(&a.rrf));
    out
}

/// Fold results that say the same thing into the highest-ranked one.
///
/// Two stores often hold one fact — a note written from a conversation the
/// graph also ingested. Both would otherwise spend prompt budget on it. The
/// survivor keeps the better rank and adds the other's score: agreement is
/// evidence. Returns the merged list (re-sorted) and how many were folded.
pub fn merge_near_duplicates(fused: Vec<Fused>, overlap: f64) -> (Vec<Fused>, usize) {
    let mut kept: Vec<(Fused, HashSet<String>)> = Vec::with_capacity(fused.len());
    let mut merged = 0usize;
    'next: for item in fused {
        let terms = terms_of(&item.hit);
        if terms.len() >= MIN_TERMS_FOR_OVERLAP {
            for (keeper, keeper_terms) in kept.iter_mut() {
                if keeper_terms.len() >= MIN_TERMS_FOR_OVERLAP
                    && overlap_coefficient(&terms, keeper_terms) >= overlap
                {
                    keeper.rrf += item.rrf;
                    keeper.sources.extend(item.sources.iter().copied());
                    merged += 1;
                    continue 'next;
                }
            }
        }
        kept.push((item, terms));
    }
    let mut out: Vec<Fused> = kept.into_iter().map(|(f, _)| f).collect();
    out.sort_by(|a, b| b.rrf.total_cmp(&a.rrf));
    (out, merged)
}

/// Lower-cased alphanumeric words; every CJK character stands alone because
/// those scripts have no spaces to split on.
fn terms_of(hit: &Hit) -> HashSet<String> {
    let mut terms = HashSet::new();
    for text in [&hit.header, &hit.body] {
        let mut word = String::new();
        for c in text.chars() {
            if is_cjk(c) {
                end_word(&mut word, &mut terms);
                terms.insert(c.to_string());
            } else if c.is_alphanumeric() {
                word.extend(c.to_lowercase());
            } else {
                end_word(&mut word, &mut terms);
            }
        }
        end_word(&mut word, &mut terms);
    }
    terms
}

/// One-character words ("a", "I", a stray digit) carry no identity.
fn end_word(word: &mut String, terms: &mut HashSet<String>) {
    if word.chars().count() >= 2 {
        terms.insert(std::mem::take(word));
    } else {
        word.clear();
    }
}

/// `|A ∩ B| / min(|A|, |B|)`: 1.0 when the shorter text is wholly contained
/// in the longer, which is how a snippet relates to the chunk it came from.
fn overlap_coefficient(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let smaller = a.len().min(b.len());
    if smaller == 0 {
        return 0.0;
    }
    let shared = a.iter().filter(|t| b.contains(*t)).count();
    shared as f64 / smaller as f64
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // kana
        | 0x3400..=0x4DBF // CJK extension A
        | 0x4E00..=0x9FFF // CJK unified
        | 0xAC00..=0xD7AF // hangul
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(arm: &'static str, key: &str, body: &str) -> Hit {
        Hit {
            arm,
            key: key.into(),
            header: String::new(),
            body: body.into(),
            at: None,
        }
    }

    #[test]
    fn a_list_alone_scores_by_reciprocal_rank() {
        let fused = reciprocal_rank_fusion(&[("s", vec![hit("a", "x", ""), hit("a", "y", "")])]);
        assert_eq!(fused.len(), 2);
        assert!((fused[0].rrf - 1.0 / 61.0).abs() < 1e-12);
        assert!((fused[1].rrf - 1.0 / 62.0).abs() < 1e-12);
        assert_eq!(fused[0].sources, vec![("s", 1)]);
    }

    #[test]
    fn the_same_item_in_two_strategies_is_one_result_with_both_scores() {
        // Rank 1 by one strategy and rank 5 by another: 1/61 + 1/65.
        let keyword = vec![hit("facts", "f1", ""), hit("facts", "f2", "")];
        let vector = vec![
            hit("facts", "f9", ""),
            hit("facts", "f8", ""),
            hit("facts", "f7", ""),
            hit("facts", "f6", ""),
            hit("facts", "f1", ""),
        ];
        let fused = reciprocal_rank_fusion(&[("keyword", keyword), ("vector", vector)]);
        assert_eq!(fused.len(), 6);
        assert_eq!(
            fused[0].hit.key, "f1",
            "agreement outranks a lone first place"
        );
        assert!((fused[0].rrf - (1.0 / 61.0 + 1.0 / 65.0)).abs() < 1e-12);
        assert_eq!(fused[0].sources, vec![("keyword", 1), ("vector", 5)]);
    }

    #[test]
    fn identity_includes_the_store_so_equal_keys_in_different_stores_stay_apart() {
        // A file path and a node id that happened to read the same are two things.
        let fused = reciprocal_rank_fusion(&[
            (
                "curated",
                vec![hit("curated", "same", ""), hit("curated", "x", "")],
            ),
            ("cognitive", vec![hit("cognitive", "same", "")]),
        ]);
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn the_same_key_twice_in_one_list_counts_once() {
        let fused = reciprocal_rank_fusion(&[(
            "s",
            vec![hit("a", "x", ""), hit("a", "x", ""), hit("a", "y", "")],
        )]);
        assert_eq!(fused.len(), 2);
        // y keeps its true list position, rank 3.
        assert!((fused[1].rrf - 1.0 / 63.0).abs() < 1e-12);
    }

    #[test]
    fn ties_keep_list_order() {
        let fused = reciprocal_rank_fusion(&[
            ("a", vec![hit("a", "x", "")]),
            ("b", vec![hit("b", "y", "")]),
        ]);
        assert_eq!(fused[0].hit.key, "x");
        assert_eq!(fused[1].hit.key, "y");
    }

    #[test]
    fn a_snippet_contained_in_a_chunk_is_folded_and_adds_its_score() {
        let note = hit(
            "curated",
            "n",
            "The daemon serves the web UI on port 18788 and the websocket gateway on port 18789",
        );
        let chunk = hit(
            "cognitive",
            "c",
            "the daemon serves the web UI on port 18788 and the websocket gateway on port 18789 by default",
        );
        let fused = reciprocal_rank_fusion(&[("curated", vec![note]), ("cognitive", vec![chunk])]);
        let (out, merged) = merge_near_duplicates(fused, DEFAULT_DUPLICATE_OVERLAP);
        assert_eq!(merged, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].sources.len(), 2);
        assert!((out[0].rrf - 2.0 / 61.0).abs() < 1e-12);
    }

    fn merged(a: Hit, b: Hit) -> (Vec<Fused>, usize) {
        let arms = [a.arm, b.arm];
        merge_near_duplicates(
            reciprocal_rank_fusion(&[(arms[0], vec![a]), (arms[1], vec![b])]),
            DEFAULT_DUPLICATE_OVERLAP,
        )
    }

    #[test]
    fn unrelated_items_are_never_folded() {
        let (out, folded) = merged(
            hit(
                "curated",
                "a",
                "scheduler polls due cron tasks every second and runs each in isolation",
            ),
            hit(
                "cognitive",
                "b",
                "telegram pairing answers an unknown chat with an eight character code",
            ),
        );
        assert_eq!(folded, 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_short_line_is_not_swallowed_by_a_long_note_that_mentions_it() {
        let (_, folded) = merged(
            hit("curated", "s", "port 18788 default"),
            hit(
                "cognitive",
                "l",
                "port 18788 default for the web UI and many other unrelated words fill this long note",
            ),
        );
        assert_eq!(folded, 0, "fewer than six terms carries no evidence");
    }

    #[test]
    fn cjk_text_is_compared_character_by_character() {
        let (out, folded) = merged(
            hit("curated", "a", "守护进程在端口一八七八八上提供网页界面服务"),
            hit(
                "cognitive",
                "b",
                "守护进程在端口一八七八八上提供网页界面服务并且还有更多内容",
            ),
        );
        assert_eq!(folded, 1);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn merging_reorders_by_the_summed_score() {
        // `solo` is rank 1 of its own list; the duplicate pair outranks it
        // once their scores add.
        let solo = hit(
            "curated",
            "solo",
            "completely different subject matter about browsers and tabs only",
        );
        let dup_a = hit(
            "curated",
            "da",
            "the quick brown fox jumps over the lazy dog near the river bank today",
        );
        let dup_b = hit(
            "cognitive",
            "db",
            "the quick brown fox jumps over the lazy dog near the river bank today",
        );
        let fused =
            reciprocal_rank_fusion(&[("curated", vec![solo, dup_a]), ("cognitive", vec![dup_b])]);
        // Before merging: solo 1/61, db 1/61 (first of its list), da 1/62.
        let (out, folded) = merge_near_duplicates(fused, DEFAULT_DUPLICATE_OVERLAP);
        assert_eq!(folded, 1);
        // The better-ranked twin survives (db, ahead of da) and now carries
        // 1/61 + 1/62, which beats solo's 1/61.
        assert_eq!(out[0].hit.key, "db");
        assert!((out[0].rrf - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-12);
        assert_eq!(out[1].hit.key, "solo");
    }
}

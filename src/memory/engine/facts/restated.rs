//! A fact the model said again, told apart from one that changed.
//!
//! The extractor is shown the stored facts beside each chunk and told not to
//! state them again. A small model does anyway, in other words ("thích sử dụng
//! React" for "thích dùng React", "User" for "Người dùng"). With an embedding
//! model such a copy is caught by its vector; without one — the default
//! install — only the exact text is, and the copy is stored as a second fact.
//!
//! Dropping a copy is cheap to get wrong in the expensive direction: "Sen lives
//! in Huế" is eight-ninths the words of "Sen lives in Hà Nội", and storing both
//! is harmless while losing the move is not. So a new fact is a restatement of
//! a known one only when every one of these holds:
//!
//! - it claims no `replaces` (a fact that says what it replaces says it changed);
//! - it has at least [`MIN_WORDS`] words and [`RESTATED_SHARE`] of them are the
//!   known fact's: it adds a few words, not a clause. Not
//!   [`overlap_coefficient`](crate::memory::engine::fusion::overlap_coefficient),
//!   which divides by the *shorter* text and so scores 1.0 for a long fact that
//!   merely contains a short stored one while adding something to it;
//! - everything it names, numbers or negates is in the known fact too — dates
//!   and numbers, negations, names (a capitalised word that does not open a
//!   later sentence) — and it keeps the negations the known fact has. One
//!   number, "không" or name apart is a different claim; a copy that merely
//!   leaves a name or a number out says less, and dropping a "không" says the
//!   opposite;
//! - every entity the model listed, on either side, is still mostly present in
//!   the other fact (this catches the lower-case "dark mode" → "light mode");
//! - its `when`, if it has one, is the known fact's or the known fact has none.
//!
//! "user", "assistant" and their Vietnamese forms are the subject of nearly
//! every fact, so they are not counted as words at all.
//!
//! What this cannot see is a swap of one lower-case word nobody listed as an
//! entity ("prefers dark mode" → "prefers light mode" in a long sentence). The
//! 0.8 share is the margin that keeps most such swaps below the line, not a
//! proof; a model that notices a change says `replaces`, which this respects.
//!
//! The same rule runs between the facts of ONE reply ([`without_restated_siblings`]):
//! a small model often states a thing twice in a reply, once in a line and once
//! with everything it knows ("đã sửa lỗi đăng nhập của ứng dụng thanh toán" and
//! "Hôm qua, … bằng cách xóa cookie cũ …, mất khoảng hai tiếng"). Every fact of
//! the reply is stored only after the whole reply is parsed, so the first of two
//! siblings is not yet a stored fact when the second is judged — and when the
//! short one comes first it would be stored before the long one shows it adds
//! nothing. The siblings are therefore compared as they were said, before
//! anything is stored. The short fact is the one left out, whichever came first.
//! Stricter than against a stored fact: a sibling is left out only when EVERY
//! word of it is in the other ([`SIBLING_SHARE`]) — there is no stored wording
//! the model could have drifted from, and a swapped lower-case word ("buổi tối"
//! → "buổi sáng"), which nothing else can see, must not cost a fact. A sibling is
//! never left out for one that says less, and a dated fact is never left out for
//! an undated one (nobody would carry its date over).

use std::cmp::Reverse;
use std::collections::HashSet;

use super::extract::Extracted;
use super::retain::GENERIC_SUBJECTS;
use super::store::StoredFact;
use crate::memory::engine::fusion::text_terms;

/// Share of the new fact's words the known fact already has.
pub const RESTATED_SHARE: f64 = 0.8;
/// Share of a sibling's words the other sibling must have: all of them.
const SIBLING_SHARE: f64 = 1.0;
/// A shorter statement says too little to be recognised by its words.
const MIN_WORDS: usize = 3;
/// Words that turn a statement around. Lower-case, as `text_terms` writes them;
/// the English contractions are the stems left when the apostrophe splits them.
const NEGATIONS: [&str; 31] = [
    "không", "chẳng", "chả", "chưa", "đừng", "chớ", "bỏ", "ngừng", "dừng", "hủy", "huỷ", "not",
    "no", "never", "stop", "stopped", "quit", "cannot", "don", "doesn", "didn", "isn", "aren",
    "wasn", "weren", "won", "wouldn", "couldn", "shouldn", "hasn", "haven",
];

/// The known fact `new` only says again, best match first; `None` when `new`
/// adds or changes something.
pub(super) fn restated_by<'a>(known: &'a [StoredFact], new: &Extracted) -> Option<&'a StoredFact> {
    if !new.replaces.is_empty() {
        return None;
    }
    let mine = Claim::said(new);
    known
        .iter()
        .filter_map(|old| {
            restated_share(&mine, &Claim::stored(old), RESTATED_SHARE).map(|share| (old, share))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(old, _)| old)
}

/// The facts of one model reply without those that only say another fact of the
/// same reply again, in the order they were said, and how many were left out.
///
/// Facts are judged longest first, each against the ones already kept: a fact is
/// left out for a sibling that says as much or more, never for one that says
/// less, and the sibling it is left out for is always one that stays. Of two
/// that say the same, the one said first stays. A fact that names what it
/// replaces is a claim of change: it always stays and is judged first, so a plain
/// copy of it is the one left out (the replacement is only applied to a fact that
/// is stored new).
pub(super) fn without_restated_siblings(facts: Vec<Extracted>) -> (Vec<Extracted>, usize) {
    if facts.len() < 2 {
        return (facts, 0);
    }
    let kept = {
        let claims: Vec<Claim> = facts.iter().map(Claim::said).collect();
        let mut order: Vec<usize> = (0..facts.len()).collect();
        order.sort_by_key(|&i| {
            (
                facts[i].replaces.is_empty(),
                Reverse(claims[i].profile.terms.len()),
                i,
            )
        });
        let mut kept = vec![false; facts.len()];
        for i in order {
            kept[i] = !facts[i].replaces.is_empty()
                || !(0..facts.len())
                    .any(|k| k != i && kept[k] && says_again(&claims[i], &claims[k]));
        }
        kept
    };
    let total = facts.len();
    let facts: Vec<Extracted> = facts
        .into_iter()
        .zip(kept)
        .filter_map(|(fact, kept)| kept.then_some(fact))
        .collect();
    let left_out = total - facts.len();
    (facts, left_out)
}

/// Does `new` add nothing to its sibling `other`? Unlike a stored fact, a
/// sibling is not given a date it lacks, so `new` may not be the dated one.
fn says_again(new: &Claim, other: &Claim) -> bool {
    restated_share(new, other, SIBLING_SHARE).is_some()
        && (new.when.is_none() || other.when.is_some())
}

/// What the check reads of one statement, whichever side of it is on.
struct Claim<'a> {
    profile: Profile,
    /// The entities the model listed, as [`entity_key`](super::store::entity_key)s.
    entities: &'a [String],
    /// First day of its date.
    when: Option<i64>,
}

impl<'a> Claim<'a> {
    fn stored(fact: &'a StoredFact) -> Self {
        Self {
            profile: profile(&fact.text),
            entities: &fact.entities,
            when: fact.occurred_start,
        }
    }

    fn said(fact: &'a Extracted) -> Self {
        Self {
            profile: profile(&fact.text),
            entities: &fact.entities,
            when: fact.occurred.map(|(start, _)| start),
        }
    }
}

/// How much of `new` is `old`'s, when at least `min_share` of it is and it adds
/// or changes nothing to `old`.
fn restated_share(new: &Claim, old: &Claim, min_share: f64) -> Option<f64> {
    let (mine, theirs) = (&new.profile, &old.profile);
    if mine.terms.len() < MIN_WORDS {
        return None;
    }
    let share = mine.terms.intersection(&theirs.terms).count() as f64 / mine.terms.len() as f64;
    (share >= min_share
        && mine.identity.is_subset(&theirs.terms)
        && theirs.negations.is_subset(&mine.terms)
        && entities_present(new.entities, &theirs.terms)
        && entities_present(old.entities, &mine.terms)
        && same_time(new.when, old.when))
    .then_some(share)
}

/// A repeat may leave the date out, or give one the known fact lacks (the
/// caller then adds it); it may not give a different one.
fn same_time(new: Option<i64>, old: Option<i64>) -> bool {
    match (new, old) {
        (Some(new), Some(old)) => new == old,
        _ => true,
    }
}

/// More than half of each non-generic entity's words appear in the other fact.
fn entities_present(entities: &[String], other: &HashSet<String>) -> bool {
    entities
        .iter()
        .filter(|e| !GENERIC_SUBJECTS.contains(&e.as_str()))
        .all(|e| {
            let parts = text_terms(e);
            parts.is_empty()
                || parts.iter().filter(|p| other.contains(*p)).count() * 2 > parts.len()
        })
}

struct Profile {
    /// Comparable words, generic subjects left out.
    terms: HashSet<String>,
    /// The ones of those that name a thing, a number or a negation.
    identity: HashSet<String>,
    /// The negations among them.
    negations: HashSet<String>,
}

fn profile(text: &str) -> Profile {
    let kept = without_generic_subjects(words(text));
    let joined = kept
        .iter()
        .map(|w| w.lower.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let negations: HashSet<String> = kept
        .iter()
        .filter(|w| NEGATIONS.contains(&w.lower.as_str()))
        .map(|w| w.lower.clone())
        .collect();
    let mut identity = negations.clone();
    identity.extend(
        kept.iter()
            .filter(|w| {
                w.lower.chars().count() >= 2
                    && (w.name_like || w.lower.chars().any(char::is_numeric))
            })
            .map(|w| w.lower.clone()),
    );
    Profile {
        terms: text_terms(&joined),
        identity,
        negations,
    }
}

#[derive(Clone)]
struct Word {
    lower: String,
    /// Capitalised, and not just because a sentence began there. The first word
    /// counts: "Emily" and "Anna" must not read as the same subject.
    name_like: bool,
}

fn words(text: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut after_stop = false;
    for c in text.chars().chain(std::iter::once(' ')) {
        if c.is_alphanumeric() {
            word.push(c);
            continue;
        }
        if let Some(first) = word.chars().next() {
            out.push(Word {
                lower: word.to_lowercase(),
                name_like: first.is_uppercase() && !after_stop,
            });
            word.clear();
            after_stop = false;
        }
        after_stop |= matches!(c, '.' | '!' | '?' | '\n');
    }
    out
}

fn without_generic_subjects(words: Vec<Word>) -> Vec<Word> {
    let mut out = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let skip = GENERIC_SUBJECTS
            .iter()
            .map(|g| g.split(' ').collect::<Vec<_>>())
            .find(|parts| {
                words.len() - i >= parts.len()
                    && parts.iter().zip(&words[i..]).all(|(p, w)| *p == w.lower)
            })
            .map_or(0, |parts| parts.len());
        if skip > 0 {
            i += skip;
        } else {
            out.push(words[i].clone());
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::engine::facts::store::{entity_key, FactKind};

    const DAY: i64 = 86_400;
    /// 2026-10-15 00:00 UTC.
    const OCT_15: i64 = 1_792_022_400;

    fn stored(text: &str, entities: &[&str], when: Option<i64>) -> StoredFact {
        StoredFact {
            id: "f1".into(),
            kind: FactKind::World,
            text: text.into(),
            occurred_start: when,
            occurred_end: when.map(|w| w + DAY),
            mentioned_at: 0,
            entities: entities.iter().map(|e| entity_key(e)).collect(),
            proof_count: 1,
            superseded_by: None,
            origin: String::new(),
            slot: None,
            source: String::new(),
        }
    }

    fn said(text: &str, entities: &[&str], when: Option<i64>) -> Extracted {
        Extracted {
            slot: None,
            text: text.into(),
            kind: FactKind::World,
            occurred: when.map(|w| (w, w + DAY)),
            entities: entities.iter().map(|e| entity_key(e)).collect(),
            replaces: Vec::new(),
        }
    }

    /// Is `new` a restatement of the one fact `old`?
    fn restates(old: &StoredFact, new: &Extracted) -> bool {
        restated_by(std::slice::from_ref(old), new).is_some()
    }

    // What the 2B model really did in the real-extractor eval: the same fact in
    // other words.

    #[test]
    fn a_reworded_copy_is_a_restatement() {
        let cases = [
            // a particle added
            ("Emily chuyển nhà đến Huế.", "Emily đã chuyển nhà đến Huế."),
            // "khóa" said as "key"
            (
                "Người dùng sử dụng một khóa API cho dự án Orion.",
                "Người dùng sử dụng một API key cho dự án Orion.",
            ),
            // "Người dùng" said as "User"
            (
                "User đã xác nhận thông tin từ Assistant.",
                "Người dùng đã xác nhận thông tin từ Assistant.",
            ),
            // a sentence split in two
            (
                "Hôm qua, người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ và cấu hình lại phiên. Việc này mất khoảng hai tiếng.",
                "Hôm qua, người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ và cấu hình lại phiên, mất khoảng hai tiếng.",
            ),
            // the same with a clause dropped: it says less than what is stored
            (
                "Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ và cấu hình lại phiên, mất khoảng hai tiếng.",
                "Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán.",
            ),
            // English
            (
                "The user prefers dark mode in the editor.",
                "The user really prefers dark mode in the editor.",
            ),
            // a copy that leaves out a number or a date word says less
            (
                "Hôm qua, Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm sang 4802 và khởi động lại daemon.",
                "Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm và khởi động lại daemon.",
            ),
        ];
        for (old, new) in cases {
            assert!(
                restates(&stored(old, &[], None), &said(new, &[], None)),
                "{new}"
            );
        }
    }

    #[test]
    fn the_closest_known_fact_is_the_one_restated() {
        let known = [
            stored("Emily chuyển nhà đến Huế.", &[], None),
            stored("Emily đã chuyển nhà đến Huế rồi.", &[], None),
        ];
        let got = restated_by(&known, &said("Emily đã chuyển nhà đến Huế.", &[], None)).unwrap();
        assert_eq!(got.text, "Emily đã chuyển nhà đến Huế rồi.");
    }

    // What must not be swallowed.

    #[test]
    fn a_changed_mind_is_not_a_restatement() {
        let react = stored(
            "Người dùng thích dùng React cho frontend.",
            &["user", "React"],
            None,
        );
        for new in [
            // the fixture's replacement
            "Người dùng đã chuyển sang Vue và không còn sử dụng React cho frontend.",
            // one name apart, with and without the model listing it
            "Người dùng thích dùng Vue cho frontend.",
        ] {
            assert!(
                !restates(&react, &said(new, &["user", "Vue"], None)),
                "{new}"
            );
            assert!(!restates(&react, &said(new, &[], None)), "{new}");
        }
    }

    #[test]
    fn one_name_number_or_negation_apart_is_a_different_claim() {
        let cases = [
            // the subject
            ("Emily chuyển nhà đến Huế.", "Anna chuyển nhà đến Huế."),
            // the place, in the middle of a longer sentence
            (
                "Sen sống ở Hà Nội và làm việc tại FPT.",
                "Sen sống ở Huế và làm việc tại FPT.",
            ),
            // a port
            (
                "Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm sang 4802 và khởi động lại daemon.",
                "Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm sang 4803 và khởi động lại daemon.",
            ),
            // a date inside the text
            (
                "Dự án Orion dự kiến triển khai vào ngày 2026-11-02.",
                "Dự án Orion dự kiến triển khai vào ngày 2026-11-09.",
            ),
            ("Lan đang nghỉ phép đến hết tháng 11.", "Lan đang nghỉ phép đến hết tháng 12."),
            // a negation added, and taken away
            (
                "Người dùng thích dùng React cho frontend.",
                "Người dùng không thích dùng React cho frontend.",
            ),
            (
                "Người dùng không còn thích dùng React cho frontend nữa.",
                "Người dùng còn thích dùng React cho frontend nữa.",
            ),
            ("The user lives in Hanoi with Sen.", "The user lives in Hue with Sen."),
            (
                "The assistant did not restart the daemon after the change.",
                "The assistant restarted the daemon after the change.",
            ),
        ];
        for (old, new) in cases {
            assert!(
                !restates(&stored(old, &[], None), &said(new, &[], None)),
                "{new}"
            );
        }
    }

    #[test]
    fn a_swapped_thing_the_model_listed_as_an_entity_is_a_different_claim() {
        let old = stored(
            "The user prefers dark mode in the editor.",
            &["user", "dark mode"],
            None,
        );
        let new = "The user prefers light mode in the editor.";
        assert!(!restates(&old, &said(new, &["user", "light mode"], None)));
        // Listed only on the stored side: the new text lost "dark mode".
        assert!(!restates(&old, &said(new, &[], None)));
        // A concept the model merely reworded, listed in other words, is not a
        // change: most of each entity's words are still there.
        let app = stored(
            "Người dùng đã sửa lỗi đăng nhập của app thanh toán.",
            &["user", "app thanh toán"],
            None,
        );
        let again = said(
            "Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán.",
            &["user", "thanh toán"],
            None,
        );
        assert!(restates(&app, &again));
    }

    #[test]
    fn a_copy_that_adds_a_date_a_name_or_a_number_says_more() {
        let cases = [
            (
                "Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ.",
                "Hôm qua, người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ.",
            ),
            (
                "Người dùng sử dụng một API key cho dự án.",
                "Người dùng sử dụng một API key cho dự án Orion.",
            ),
            (
                "Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm và khởi động lại daemon.",
                "Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm sang 4802 và khởi động lại daemon.",
            ),
        ];
        for (old, new) in cases {
            assert!(
                !restates(&stored(old, &[], None), &said(new, &[], None)),
                "{new}"
            );
        }
    }

    #[test]
    fn a_fact_that_adds_a_clause_is_new() {
        let old = stored("Emily chuyển nhà đến Huế.", &[], None);
        let new = said(
            "Emily chuyển nhà đến Huế và làm việc tại ngân hàng.",
            &[],
            None,
        );
        assert!(!restates(&old, &new), "the stored fact is wholly inside it");
    }

    #[test]
    fn a_fact_that_says_what_it_replaces_is_never_a_restatement() {
        let old = stored("Người dùng thích dùng React cho frontend.", &[], None);
        let mut new = said("Người dùng rất thích dùng React cho frontend.", &[], None);
        assert!(restates(&old, &new));
        new.replaces = vec![1];
        assert!(!restates(&old, &new));
    }

    #[test]
    fn a_fragment_is_not_recognised_by_its_words() {
        let old = stored("Sen thích trà xanh vào buổi tối.", &[], None);
        assert!(!restates(&old, &said("Sen thích.", &[], None)));
        assert!(restates(&old, &said("Sen thích trà xanh.", &[], None)));
    }

    // Dates.

    #[test]
    fn a_repeat_may_leave_out_the_date_or_bring_one() {
        let text = "Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán.";
        let dated = stored(text, &[], Some(OCT_15));
        assert!(restates(&dated, &said(text, &[], None)), "left out");
        assert!(restates(&dated, &said(text, &[], Some(OCT_15))), "same day");
        let undated = stored(text, &[], None);
        assert!(
            restates(&undated, &said(text, &[], Some(OCT_15))),
            "the caller gives the stored fact the date"
        );
    }

    #[test]
    fn the_same_words_on_another_day_are_another_event() {
        let text = "Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán.";
        let old = stored(text, &[], Some(OCT_15));
        assert!(!restates(&old, &said(text, &[], Some(OCT_15 + 5 * DAY))));
    }

    // Two facts of one reply.

    const SHORT_LOGIN: &str = "Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán.";
    const LONG_LOGIN: &str = "Hôm qua, người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ và cấu hình lại phiên, mất khoảng hai tiếng.";

    /// What the reply keeps (texts, in order) and how many it left out.
    fn after(facts: Vec<Extracted>) -> (Vec<String>, usize) {
        let (kept, left_out) = without_restated_siblings(facts);
        (kept.into_iter().map(|f| f.text).collect(), left_out)
    }

    /// Both facts, said in either order.
    fn either_order(a: Extracted, b: Extracted) -> [Vec<Extracted>; 2] {
        [vec![a.clone(), b.clone()], vec![b, a]]
    }

    #[test]
    fn a_short_fact_beside_the_detailed_one_of_the_same_reply_is_left_out() {
        // As the 2B model said them in the real-extractor eval: the line, with
        // an entity of its own, and everything it knows, dated.
        for listed in [
            &["user", "ứng dụng thanh toán"][..],
            &["app thanh toán"][..],
        ] {
            let short = said(SHORT_LOGIN, listed, None);
            let long = said(LONG_LOGIN, &["user"], Some(OCT_15));
            for reply in either_order(short, long) {
                let (kept, left_out) = after(reply);
                assert_eq!((kept, left_out), (vec![LONG_LOGIN.to_string()], 1));
            }
        }
    }

    #[test]
    fn what_the_detailed_fact_is_left_with_is_untouched() {
        let short = said(SHORT_LOGIN, &[], None);
        let long = said(LONG_LOGIN, &["user"], Some(OCT_15));
        let (kept, _) = without_restated_siblings(vec![short, long.clone()]);
        assert_eq!(kept, vec![long], "text, date, entities, kind: all as said");
    }

    #[test]
    fn a_copy_that_leaves_out_a_number_or_a_clause_says_less_than_its_sibling() {
        let long = "Hôm qua, Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm sang 4802 và khởi động lại daemon.";
        let short =
            "Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm và khởi động lại daemon.";
        for reply in either_order(said(short, &[], None), said(long, &[], None)) {
            assert_eq!(after(reply), (vec![long.to_string()], 1));
        }
        // The short fact said twice, beside the long one.
        let reply = vec![
            said("Emily chuyển nhà đến Huế.", &[], None),
            said("Emily chuyển nhà đến Huế và làm việc tại FPT.", &[], None),
            said("Emily chuyển nhà đến Huế.", &[], None),
        ];
        assert_eq!(
            after(reply),
            (
                vec!["Emily chuyển nhà đến Huế và làm việc tại FPT.".to_string()],
                2
            )
        );
    }

    #[test]
    fn of_two_facts_that_say_the_same_the_one_said_first_stays() {
        let reply = vec![
            said("Sen thích trà xanh vào buổi tối.", &[], None),
            said("Sen thích trà xanh vào buổi tối!", &[], None),
        ];
        assert_eq!(
            after(reply),
            (vec!["Sen thích trà xanh vào buổi tối.".to_string()], 1)
        );
    }

    #[test]
    fn survivors_keep_the_order_they_were_said_in() {
        let other = "Dự án Falcon do Lan phụ trách.";
        let reply = vec![
            said(SHORT_LOGIN, &[], None),
            said(other, &[], None),
            said(LONG_LOGIN, &[], Some(OCT_15)),
        ];
        assert_eq!(
            after(reply),
            (vec![other.to_string(), LONG_LOGIN.to_string()], 1)
        );
    }

    // What must survive, between siblings.

    #[test]
    fn two_different_facts_that_share_a_subject_are_both_kept() {
        let cases = [
            // from the eval: the same app, two different things said about it
            (
                "Người dùng đang làm việc liên quan đến ứng dụng thanh toán.",
                LONG_LOGIN,
            ),
            (
                "Emily đã chuyển nhà đến Huế vào tháng 3 năm nay.",
                "Emily từng làm việc tại FPT trước khi chuyển đến Huế.",
            ),
            (
                "Sen thích trà xanh vào buổi tối.",
                "Sen thích cà phê vào buổi sáng.",
            ),
            (
                "Dự án Falcon do Lan phụ trách.",
                "Lan đang nghỉ phép đến hết tháng 11.",
            ),
        ];
        for (a, b) in cases {
            for reply in either_order(said(a, &[], None), said(b, &[], None)) {
                let (kept, left_out) = after(reply);
                assert_eq!((kept.len(), left_out), (2, 0), "{a} / {b}");
            }
        }
    }

    #[test]
    fn one_name_number_or_negation_apart_keeps_both_siblings() {
        let cases = [
            ("Emily chuyển nhà đến Huế.", "Anna chuyển nhà đến Huế."),
            (
                "Sen sống ở Hà Nội và làm việc tại FPT.",
                "Sen sống ở Huế và làm việc tại FPT.",
            ),
            (
                "Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm sang 4802 và khởi động lại daemon.",
                "Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm sang 4803 và khởi động lại daemon.",
            ),
            (
                "Người dùng thích dùng React cho frontend.",
                "Người dùng không thích dùng React cho frontend.",
            ),
            // a negation only the longer one has: the shorter says the opposite
            (
                "Người dùng thích dùng React cho frontend.",
                "Người dùng không còn thích dùng React cho frontend nữa.",
            ),
        ];
        for (a, b) in cases {
            for reply in either_order(said(a, &[], None), said(b, &[], None)) {
                assert_eq!(after(reply).1, 0, "{a} / {b}");
            }
        }
    }

    #[test]
    fn a_swapped_lower_case_word_keeps_both_siblings() {
        // Against a stored fact this swap is 6 words of 7 and falls to the
        // listed-entity check, or goes unseen; between siblings every word of the
        // dropped fact must be the other's, so nothing here needs a listing.
        for (a, b) in [
            (
                "Sen thích trà xanh vào buổi tối.",
                "Sen thích trà xanh vào buổi sáng.",
            ),
            (
                "The user prefers dark mode in the editor.",
                "The user prefers light mode in the editor.",
            ),
        ] {
            for reply in either_order(said(a, &[], None), said(b, &[], None)) {
                assert_eq!(after(reply).1, 0, "{a} / {b}");
            }
        }
    }

    #[test]
    fn a_listed_entity_that_moved_between_siblings_keeps_both() {
        let dark = said(
            "The user prefers dark mode in the editor.",
            &["user", "dark mode"],
            None,
        );
        let light = said(
            "The user prefers light mode in the editor.",
            &["user", "light mode"],
            None,
        );
        for reply in either_order(dark, light) {
            assert_eq!(after(reply).1, 0);
        }
        // The guards are the stored fact's, both ways: an entity either lists must
        // be mostly in the other's words. The long fact listing a detail the short
        // one never mentions keeps both (a duplicate, not a loss).
        let reply = |short: &[&str], long: &[&str]| {
            vec![
                said(SHORT_LOGIN, short, None),
                said(LONG_LOGIN, long, Some(OCT_15)),
            ]
        };
        assert_eq!(after(reply(&[], &["user", "ứng dụng thanh toán"])).1, 1);
        assert_eq!(after(reply(&[], &["user", "cookie"])).1, 0);
        assert_eq!(after(reply(&["token"], &["user"])).1, 0);
    }

    #[test]
    fn siblings_on_different_days_are_two_events() {
        let a = said(SHORT_LOGIN, &[], Some(OCT_15));
        let b = said(SHORT_LOGIN, &[], Some(OCT_15 + 5 * DAY));
        assert_eq!(after(vec![a, b]).1, 0);
    }

    #[test]
    fn a_dated_fact_is_not_left_out_for_an_undated_sibling() {
        // Nobody would carry the date over to the one that stays.
        let short = said(SHORT_LOGIN, &[], Some(OCT_15));
        let long = said(
            "Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ.",
            &[],
            None,
        );
        for reply in either_order(short, long) {
            assert_eq!(after(reply).1, 0);
        }
        // The same day on both: one fact.
        let short = said(SHORT_LOGIN, &[], Some(OCT_15));
        let long = said(LONG_LOGIN, &[], Some(OCT_15));
        assert_eq!(after(vec![short, long]).1, 1);
    }

    #[test]
    fn a_fact_that_says_what_it_replaces_is_never_left_out_for_a_sibling() {
        let mut short = said(SHORT_LOGIN, &[], None);
        short.replaces = vec![1];
        let long = said(LONG_LOGIN, &[], Some(OCT_15));
        for reply in either_order(short.clone(), long.clone()) {
            assert_eq!(after(reply).1, 0);
        }
        // It is what a plain copy of it, said before or after, is left out for.
        let copy = said(SHORT_LOGIN, &[], None);
        for reply in either_order(copy, short.clone()) {
            let (kept, left_out) = without_restated_siblings(reply);
            assert_eq!((kept, left_out), (vec![short.clone()], 1));
        }
    }

    #[test]
    fn a_reply_of_one_or_none_is_returned_as_it_is() {
        assert_eq!(without_restated_siblings(Vec::new()), (Vec::new(), 0));
        let one = said(SHORT_LOGIN, &[], None);
        assert_eq!(without_restated_siblings(vec![one.clone()]), (vec![one], 0));
    }

    #[test]
    fn a_fragment_is_not_recognised_by_its_words_between_siblings_either() {
        let reply = vec![
            said("Sen thích trà xanh vào buổi tối.", &[], None),
            said("Sen thích.", &[], None),
        ];
        assert_eq!(after(reply).1, 0);
    }
}

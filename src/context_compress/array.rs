//! Row selection for a long JSON array — the part of Headroom's SmartCrusher
//! that matters for an agent: keep the start (it shows the shape), the end (the
//! most recent), every row that looks like trouble, and a spread through the
//! middle, and say how many were left out.
//!
//! "Looks like trouble" is decided from the rows themselves, with no model:
//! a row that names an error, a value that is rare in a field where every
//! other row agrees (`"status"` 97 × `ok` and 3 × something else), a field only
//! a few rows have, and a number far from the rest of its column.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// An array shorter than this is never cut: the saving would be small and the
/// model is more likely to need all of it.
pub const MIN_ITEMS: usize = 20;
/// Rows kept at the start and at the end.
const HEAD: usize = 5;
const TAIL: usize = 3;
/// Most rows kept for being unusual, and in total.
const MAX_ANOMALIES: usize = 7;
const MAX_KEPT: usize = 15;

/// A field present in at least this share of the rows is "common"; one in
/// fewer than `RARE_SHARE` is "rare".
const COMMON_SHARE: f64 = 0.8;
const RARE_SHARE: f64 = 0.2;
/// Standard deviations from the mean for a number to be an outlier.
const OUTLIER_SIGMAS: f64 = 3.0;

/// Words that mark a row as one the model must see. A match is at the start of
/// a word, so `failed` and `errors` count and `terror` does not.
const TROUBLE_WORDS: &[&str] = &[
    "error",
    "exception",
    "fail",
    "critical",
    "fatal",
    "crash",
    "panic",
    "abort",
    "timeout",
    "timed out",
    "denied",
    "rejected",
    "lỗi",
    "thất bại",
];

/// Why a row was kept, most important first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Why {
    Trouble,
    RareValue,
    RareField,
    Outlier,
}

/// The indices to keep, ascending. `elems` are byte ranges into `text`.
pub fn select(text: &str, elems: &[(usize, usize)]) -> Vec<usize> {
    let n = elems.len();
    let rows: Vec<Option<Value>> = elems
        .iter()
        .map(|&(s, e)| serde_json::from_str::<Value>(&text[s..e]).ok())
        .collect();

    let mut keep: BTreeSet<usize> = (0..HEAD.min(n)).chain(n.saturating_sub(TAIL)..n).collect();

    let mut flagged: BTreeMap<usize, Why> = BTreeMap::new();
    let mut flag = |i: usize, why: Why| {
        flagged
            .entry(i)
            .and_modify(|w| *w = (*w).min(why))
            .or_insert(why);
    };
    for (i, row) in rows.iter().enumerate() {
        if row.as_ref().is_some_and(is_trouble) {
            flag(i, Why::Trouble);
        }
    }
    for i in rare_values(&rows) {
        flag(i, Why::RareValue);
    }
    for i in rare_fields(&rows) {
        flag(i, Why::RareField);
    }
    for i in outliers(&rows) {
        flag(i, Why::Outlier);
    }
    let mut candidates: Vec<(Why, usize)> = flagged
        .into_iter()
        .filter(|(i, _)| !keep.contains(i))
        .map(|(i, why)| (why, i))
        .collect();
    candidates.sort();
    keep.extend(candidates.into_iter().take(MAX_ANOMALIES).map(|(_, i)| i));

    // A spread through the middle until the budget is used, so the model also
    // sees what an ordinary row looks like and roughly how the data moves.
    let free = MAX_KEPT.saturating_sub(keep.len());
    if free > 0 && n > HEAD + TAIL {
        let (lo, hi) = (HEAD, n - TAIL);
        for k in 1..=free {
            let mut i = lo + k * (hi - lo) / (free + 1);
            while i < hi && keep.contains(&i) {
                i += 1;
            }
            if i < hi {
                keep.insert(i);
            }
        }
    }
    keep.into_iter().collect()
}

fn is_trouble(row: &Value) -> bool {
    match row {
        Value::String(s) => has_trouble_word(&s.to_lowercase()),
        Value::Object(map) => map.iter().any(|(key, value)| {
            // `"error": null` and `"errors": []` on every healthy row must not
            // flag the whole array, so a trouble-named key counts only with a
            // real value behind it.
            let named = has_trouble_word(&key.to_lowercase())
                && !matches!(value, Value::Null | Value::Bool(false))
                && value.as_array().map_or(true, |a| !a.is_empty())
                && value.as_str().map_or(true, |s| !s.is_empty())
                && value.as_u64() != Some(0);
            named || is_trouble(value)
        }),
        Value::Array(items) => items.iter().any(is_trouble),
        _ => false,
    }
}

fn has_trouble_word(lower: &str) -> bool {
    TROUBLE_WORDS.iter().any(|word| {
        lower.match_indices(word).any(|(at, _)| {
            lower[..at]
                .chars()
                .next_back()
                .map_or(true, |c| !c.is_alphanumeric())
        })
    })
}

fn objects(
    rows: &[Option<Value>],
) -> impl Iterator<Item = (usize, &serde_json::Map<String, Value>)> {
    rows.iter()
        .enumerate()
        .filter_map(|(i, r)| r.as_ref().and_then(Value::as_object).map(|o| (i, o)))
}

/// A field most rows share and whose values are few: the rows with a value
/// outside the few that cover most of them.
fn rare_values(rows: &[Option<Value>]) -> Vec<usize> {
    let total = objects(rows).count();
    if total < 5 {
        return Vec::new();
    }
    let mut by_field: BTreeMap<&str, BTreeMap<String, Vec<usize>>> = BTreeMap::new();
    for (i, obj) in objects(rows) {
        for (key, value) in obj {
            let label = match value {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                _ => continue,
            };
            by_field
                .entry(key)
                .or_default()
                .entry(label)
                .or_default()
                .push(i);
        }
    }
    let mut out = Vec::new();
    for values in by_field.values() {
        let present: usize = values.values().map(Vec::len).sum();
        if (present as f64) < COMMON_SHARE * total as f64 || !(2..=10).contains(&values.len()) {
            continue;
        }
        let mut by_count: Vec<(&String, usize)> =
            values.iter().map(|(v, ix)| (v, ix.len())).collect();
        by_count.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let mut covered = 0;
        let mut top = 0;
        for (_, count) in &by_count {
            covered += count;
            top += 1;
            if covered as f64 >= COMMON_SHARE * present as f64 {
                break;
            }
        }
        if top <= 5 && top < by_count.len() {
            for (value, _) in &by_count[top..] {
                out.extend(values[*value].iter().copied());
            }
        }
    }
    out
}

/// A field only a few rows carry.
fn rare_fields(rows: &[Option<Value>]) -> Vec<usize> {
    let total = objects(rows).count();
    if total < 5 {
        return Vec::new();
    }
    let mut seen: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, obj) in objects(rows) {
        for key in obj.keys() {
            seen.entry(key).or_default().push(i);
        }
    }
    seen.values()
        .filter(|ix| (ix.len() as f64) < RARE_SHARE * total as f64)
        .flatten()
        .copied()
        .collect()
}

/// A number far from the rest of its column.
fn outliers(rows: &[Option<Value>]) -> Vec<usize> {
    let total = objects(rows).count();
    if total < 8 {
        return Vec::new();
    }
    let mut columns: BTreeMap<&str, Vec<(usize, f64)>> = BTreeMap::new();
    for (i, obj) in objects(rows) {
        for (key, value) in obj {
            if let Some(x) = value.as_f64().filter(|x| x.is_finite()) {
                columns.entry(key).or_default().push((i, x));
            }
        }
    }
    let mut out = Vec::new();
    for column in columns.values() {
        if column.len() < 8 || (column.len() as f64) < COMMON_SHARE * total as f64 {
            continue;
        }
        let n = column.len() as f64;
        let mean = column.iter().map(|&(_, x)| x).sum::<f64>() / n;
        let var = column.iter().map(|&(_, x)| (x - mean).powi(2)).sum::<f64>() / n;
        let sd = var.sqrt();
        if sd > 0.0 {
            out.extend(
                column
                    .iter()
                    .filter(|&&(_, x)| ((x - mean) / sd).abs() >= OUTLIER_SIGMAS)
                    .map(|&(i, _)| i),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minified array and the ranges `select` takes.
    fn array_of(rows: Vec<Value>) -> (String, Vec<(usize, usize)>) {
        let mut text = String::from("[");
        let mut ranges = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                text.push(',');
            }
            let start = text.len();
            text.push_str(&row.to_string());
            ranges.push((start, text.len()));
        }
        text.push(']');
        (text, ranges)
    }

    fn healthy(i: usize) -> Value {
        serde_json::json!({"id": i, "status": "ok", "error": null, "latency": 100 + (i % 7), "name": format!("job {i}")})
    }

    #[test]
    fn the_start_the_end_and_a_spread_are_kept_and_the_budget_holds() {
        let (text, ranges) = array_of((0..200).map(healthy).collect());
        let keep = select(&text, &ranges);
        assert_eq!(keep.len(), MAX_KEPT);
        assert!(keep.starts_with(&[0, 1, 2, 3, 4]));
        assert!(keep.ends_with(&[197, 198, 199]));
        assert!(keep.windows(2).all(|w| w[0] < w[1]), "ascending and unique");
        let middle: Vec<_> = keep.iter().filter(|&&i| (5..197).contains(&i)).collect();
        assert!(
            middle.len() >= 5 && middle.windows(2).all(|w| w[1] - w[0] > 5),
            "an even spread: {middle:?}"
        );
    }

    #[test]
    fn a_row_that_names_a_failure_is_always_kept() {
        let mut rows: Vec<Value> = (0..200).map(healthy).collect();
        rows[88] = serde_json::json!({"id": 88, "status": "ok", "error": "connection refused: timeout after 30s"});
        rows[141] =
            serde_json::json!({"id": 141, "status": "ok", "message": "Build failed on step 3"});
        let (text, ranges) = array_of(rows);
        let keep = select(&text, &ranges);
        assert!(keep.contains(&88) && keep.contains(&141), "{keep:?}");
    }

    #[test]
    fn a_null_error_field_on_every_row_does_not_flag_every_row() {
        let (text, ranges) = array_of((0..120).map(healthy).collect());
        let keep = select(&text, &ranges);
        assert_eq!(keep.len(), MAX_KEPT);
        // Were each row "trouble" the anomaly budget would be spent on the
        // first rows instead of a spread, and the middle would be empty.
        assert!(keep.iter().any(|&i| (20..100).contains(&i)));
    }

    #[test]
    fn a_rare_value_in_a_field_where_the_rest_agree_is_kept() {
        let mut rows: Vec<Value> = (0..100).map(healthy).collect();
        rows[37]["status"] = serde_json::json!("degraded");
        let (text, ranges) = array_of(rows);
        assert!(select(&text, &ranges).contains(&37));
    }

    #[test]
    fn a_field_only_one_row_has_is_kept() {
        let mut rows: Vec<Value> = (0..100).map(healthy).collect();
        rows[63]["note"] = serde_json::json!("manual override");
        let (text, ranges) = array_of(rows);
        assert!(select(&text, &ranges).contains(&63));
    }

    #[test]
    fn a_number_far_from_its_column_is_kept() {
        let mut rows: Vec<Value> = (0..100).map(healthy).collect();
        rows[52]["latency"] = serde_json::json!(90_000);
        let (text, ranges) = array_of(rows);
        assert!(select(&text, &ranges).contains(&52));
    }

    #[test]
    fn trouble_is_ranked_ahead_of_the_other_reasons_when_the_budget_runs_out() {
        let mut rows: Vec<Value> = (0..300).map(healthy).collect();
        // Ten oddities of a lesser kind, early; one real failure, late.
        for i in 10..20 {
            rows[i]["note"] = serde_json::json!("odd");
        }
        rows[250]["error"] = serde_json::json!("fatal: disk full");
        let (text, ranges) = array_of(rows);
        let keep = select(&text, &ranges);
        assert!(keep.contains(&250), "{keep:?}");
        assert!(keep.len() <= MAX_KEPT + MAX_ANOMALIES);
    }

    #[test]
    fn a_trouble_word_matches_at_the_start_of_a_word_only() {
        assert!(has_trouble_word("two errors found"));
        assert!(has_trouble_word("the build failed"));
        assert!(has_trouble_word("lỗi kết nối"));
        assert!(!has_trouble_word("a terrorist-free zone"));
        assert!(!has_trouble_word("all good"));
    }
}

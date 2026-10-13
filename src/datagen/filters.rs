//! Cheap, deterministic gates a candidate passes before any model judges it.
//!
//! Every rejection carries a reason code, so a manifest can say *why* a run
//! produced fewer records than asked — "800 too_similar" points at the seeds,
//! "800 label_mismatch" at the categories.
//!
//! Banned phrases match whole words, not substrings: CAMEL's self-instruct
//! filter bans "data" as a substring, which also throws away every
//! "database" and "metadata" request.

use std::collections::HashSet;

/// Words of `s`, lower-cased, split on anything that is not a letter or a
/// digit. Vietnamese diacritics are letters, so "dữ liệu" stays two words.
pub fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Length of the longest common subsequence of two token sequences.
pub fn lcs_len(a: &[String], b: &[String]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for x in a {
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = if x == y { prev[j] + 1 } else { cur[j].max(prev[j + 1]) };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// ROUGE-L F-measure between two token sequences (longest common
/// subsequence, precision and recall weighted equally).
pub fn rouge_l(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let lcs = lcs_len(a, b) as f64;
    if lcs == 0.0 {
        return 0.0;
    }
    let p = lcs / b.len() as f64;
    let r = lcs / a.len() as f64;
    2.0 * p * r / (p + r)
}

/// What a new candidate is compared against: the seeds and everything
/// accepted so far (plus, within one batch, the candidates already let
/// through, so a batch cannot accept the same request twice).
#[derive(Default)]
pub struct Known {
    exact: HashSet<String>,
    tokens: Vec<Vec<String>>,
}

impl Known {
    pub fn add(&mut self, tokens: Vec<String>) {
        self.exact.insert(tokens.join(" "));
        self.tokens.push(tokens);
    }
}

#[derive(Debug, Clone)]
pub struct FilterChain {
    pub min_words: usize,
    pub max_words: usize,
    /// Each phrase pre-tokenized; a candidate containing the token sequence
    /// is rejected.
    banned: Vec<Vec<String>>,
    /// Reject when ROUGE-L against anything known reaches this.
    pub max_similarity: f64,
}

impl FilterChain {
    pub fn new(min_words: usize, max_words: usize, banned: &[String], max_similarity: f64) -> Self {
        FilterChain {
            min_words,
            max_words: max_words.max(min_words),
            banned: banned.iter().map(|p| tokenize(p)).filter(|t| !t.is_empty()).collect(),
            max_similarity,
        }
    }

    /// The candidate's tokens when it passes, else the reason code. Checks
    /// run cheapest first and stop at the first failure.
    pub fn check(&self, text: &str, known: &Known) -> Result<Vec<String>, &'static str> {
        let tokens = tokenize(text);
        if tokens.is_empty() {
            return Err("empty");
        }
        if tokens.len() < self.min_words {
            return Err("too_short");
        }
        if tokens.len() > self.max_words {
            return Err("too_long");
        }
        if self.banned.iter().any(|b| contains_seq(&tokens, b)) {
            return Err("banned_phrase");
        }
        if known.exact.contains(&tokens.join(" ")) {
            return Err("duplicate");
        }
        if known.tokens.iter().any(|k| rouge_l(k, &tokens) >= self.max_similarity) {
            return Err("too_similar");
        }
        Ok(tokens)
    }
}

fn contains_seq(hay: &[String], needle: &[String]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain() -> FilterChain {
        FilterChain::new(3, 20, &["data".to_string(), "mật khẩu".to_string()], 0.7)
    }

    #[test]
    fn tokenizes_vietnamese_as_words() {
        assert_eq!(tokenize("Tạo dữ-liệu, NGAY!"), vec!["tạo", "dữ", "liệu", "ngay"]);
    }

    #[test]
    fn rouge_l_is_one_for_identical_and_zero_for_disjoint() {
        let a = tokenize("book a table for two");
        assert!((rouge_l(&a, &a) - 1.0).abs() < 1e-9);
        assert_eq!(rouge_l(&a, &tokenize("weather in hanoi")), 0.0);
        let b = tokenize("book a table for four tonight");
        let s = rouge_l(&a, &b);
        assert!(s > 0.5 && s < 1.0, "{s}");
    }

    #[test]
    fn banned_phrases_match_whole_words_only() {
        let known = Known::default();
        assert_eq!(chain().check("export the data to csv", &known), Err("banned_phrase"));
        assert!(chain().check("back up the database tonight", &known).is_ok());
        assert_eq!(chain().check("đổi mật khẩu wifi giúp tôi", &known), Err("banned_phrase"));
    }

    #[test]
    fn rejects_length_duplicates_and_near_duplicates() {
        let mut known = Known::default();
        known.add(tokenize("summarise this article in three bullet points"));
        let c = chain();
        assert_eq!(c.check("", &known), Err("empty"));
        assert_eq!(c.check("hi there", &known), Err("too_short"));
        assert_eq!(c.check(&"word ".repeat(21), &known), Err("too_long"));
        assert_eq!(c.check("Summarise this article in three bullet points.", &known), Err("duplicate"));
        assert_eq!(c.check("summarise this article in three short bullet points", &known), Err("too_similar"));
        assert!(c.check("plan a weekend trip to da lat", &known).is_ok());
    }
}

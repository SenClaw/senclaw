//! Tokenizer utility. Mirrors `src-old/memory/tokenizer.ts`.
//!
//! Mixed Chinese/English tokenization with optional stopword filtering.
//! Unlike the TS version, `jieba-rs` is statically linked so there is no
//! load-fallback path; the character-level fallback is kept available as a
//! utility for callers that want it (e.g. tests).

use std::collections::HashSet;
use std::sync::OnceLock;

use jieba_rs::Jieba;

use super::stopwords::filter_stopwords;

fn jieba() -> &'static Jieba {
    static INSTANCE: OnceLock<Jieba> = OnceLock::new();
    INSTANCE.get_or_init(Jieba::new)
}

fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

/// Character-level fallback: emit each char plus all 2-grams. Used when callers
/// explicitly want the no-jieba behaviour or for testing parity with the TS
/// fallback path.
pub fn cut_chinese_fallback(segment: &str) -> Vec<String> {
    let chars: Vec<char> = segment.chars().collect();
    let mut tokens: Vec<String> = chars.iter().map(|c| c.to_string()).collect();
    for w in chars.windows(2) {
        let mut s = String::with_capacity(w[0].len_utf8() + w[1].len_utf8());
        s.push(w[0]);
        s.push(w[1]);
        tokens.push(s);
    }
    tokens
}

/// Smart tokenization for mixed CJK + English text.
///
/// - CJK runs: tokenized via `jieba-rs` (`cut(.., false)` = accurate mode,
///   matches the TS default).
/// - Non-CJK runs: split on whitespace + non-alphanumeric, lowercased, kept
///   only if they contain `[a-z0-9]`.
/// - Result is deduplicated; stopwords are dropped when `remove_stopwords`.
pub fn tokenize_optimized(text: &str, remove_stopwords: bool) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }

    let mut tokens: Vec<String> = Vec::new();

    // Walk the string once, splitting into CJK runs (jieba) vs non-CJK (latin).
    let mut cjk_buf = String::new();
    let mut latin_buf = String::new();

    let flush_cjk = |buf: &mut String, out: &mut Vec<String>| {
        if buf.is_empty() {
            return;
        }
        let segment = std::mem::take(buf);
        for tok in jieba().cut(&segment, false) {
            if !tok.is_empty() {
                out.push(tok.to_lowercase());
            }
        }
    };

    let flush_latin = |buf: &mut String, out: &mut Vec<String>| {
        if buf.is_empty() {
            return;
        }
        let segment = std::mem::take(buf);
        for raw in segment.split(|c: char| c.is_whitespace() || (!c.is_alphanumeric())) {
            if raw.is_empty() {
                continue;
            }
            let lower = raw.to_lowercase();
            if lower.chars().any(|c| c.is_ascii_alphanumeric()) {
                out.push(lower);
            }
        }
    };

    for c in text.chars() {
        if is_cjk(c) {
            flush_latin(&mut latin_buf, &mut tokens);
            cjk_buf.push(c);
        } else {
            flush_cjk(&mut cjk_buf, &mut tokens);
            latin_buf.push(c);
        }
    }
    flush_cjk(&mut cjk_buf, &mut tokens);
    flush_latin(&mut latin_buf, &mut tokens);

    let mut seen: HashSet<String> = HashSet::with_capacity(tokens.len());
    let mut unique: Vec<String> = Vec::with_capacity(tokens.len());
    for t in tokens {
        if seen.insert(t.clone()) {
            unique.push(t);
        }
    }

    if remove_stopwords {
        let refs: Vec<&str> = unique.iter().map(String::as_str).collect();
        return filter_stopwords(refs);
    }
    unique
}

/// CJK 2-gram generator (used by Keyword Fallback). Mirrors `generate2gram`.
pub fn generate_2gram(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().filter(|c| is_cjk(*c)).collect();
    let mut out = Vec::with_capacity(chars.len().saturating_sub(1));
    for w in chars.windows(2) {
        let mut s = String::with_capacity(w[0].len_utf8() + w[1].len_utf8());
        s.push(w[0]);
        s.push(w[1]);
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_returns_empty() {
        assert!(tokenize_optimized("", true).is_empty());
        assert!(tokenize_optimized("   \n\t  ", true).is_empty());
    }

    #[test]
    fn english_lowercases_and_drops_punct() {
        let toks = tokenize_optimized("Hello, World! Rust-2026.", false);
        assert!(toks.contains(&"hello".to_string()));
        assert!(toks.contains(&"world".to_string()));
        assert!(toks.contains(&"rust".to_string()));
        assert!(toks.contains(&"2026".to_string()));
    }

    #[test]
    fn english_drops_stopwords() {
        let toks = tokenize_optimized("the quick brown fox", true);
        assert!(!toks.contains(&"the".to_string()));
        assert!(toks.contains(&"quick".to_string()));
    }

    #[test]
    fn mixed_cjk_and_latin() {
        let toks = tokenize_optimized("飞书 wiki 同步", false);
        assert!(toks.contains(&"wiki".to_string()));
        // jieba should produce at least one CJK segment
        assert!(toks.iter().any(|t| t.chars().any(is_cjk)));
    }

    #[test]
    fn dedupes() {
        let toks = tokenize_optimized("rust rust rust", false);
        assert_eq!(toks.iter().filter(|t| *t == "rust").count(), 1);
    }

    #[test]
    fn fallback_emits_chars_and_bigrams() {
        let toks = cut_chinese_fallback("飞书同步");
        assert!(toks.contains(&"飞".to_string()));
        assert!(toks.contains(&"飞书".to_string()));
        assert!(toks.contains(&"同步".to_string()));
    }

    #[test]
    fn bigram_only_cjk() {
        let toks = generate_2gram("飞书 wiki 同步");
        // CJK-only filtering means non-Chinese chars are stripped before bigram
        for t in &toks {
            assert!(t.chars().all(is_cjk));
        }
    }
}

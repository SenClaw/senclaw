//! Stopword list. Mirrors `src-old/memory/stopwords.ts`.
//!
//! Stopwords are terms with little/no retrieval value and should be filtered
//! after tokenization.

use std::collections::HashSet;
use std::sync::OnceLock;

const CHINESE_STOPWORDS_RAW: &[&str] = &[
    // Interrogatives
    "为什么",
    "怎么",
    "如何",
    "什么",
    "哪个",
    "哪些",
    "哪里",
    "哪儿",
    "怎样",
    "怎么样",
    "为何",
    // Particles
    "的",
    "了",
    "在",
    "是",
    "我",
    "有",
    "和",
    "就",
    "不",
    "人",
    "都",
    "一",
    "会",
    "能",
    "可以",
    "应该",
    "着",
    "过",
    "吗",
    "呢",
    "吧",
    "啊",
    "呀",
    // Conjunctions
    "和",
    "或",
    "但是",
    "然后",
    "因为",
    "所以",
    "如果",
    "虽然",
    "但",
    "而且",
    "并且",
    "或者",
    "以及",
    // Pronouns
    "我",
    "你",
    "他",
    "她",
    "它",
    "我们",
    "你们",
    "他们",
    "她们",
    "它们",
    "这",
    "那",
    "这个",
    "那个",
    "这些",
    "那些",
    "这里",
    "那里",
    // Prepositions
    "在",
    "从",
    "到",
    "对",
    "向",
    "往",
    "于",
    "给",
    "为",
    "被",
    "把",
    // Time words (generic)
    "时候",
    "时间",
    "现在",
    "以前",
    "以后",
    "之前",
    "之后",
    // Degree words
    "很",
    "非常",
    "特别",
    "十分",
    "极",
    "更",
    "最",
    "比较",
    // Others
    "等",
    "等等",
    "之类",
    "左右",
    "上下",
    "前后",
];

const ENGLISH_STOPWORDS_RAW: &[&str] = &[
    // Articles
    "the", "a", "an", // Prepositions
    "in", "on", "at", "to", "from", "by", "with", "about", "as", "into", "through", "during",
    "before", "after", "above", "below", "between", "under", "over", // Conjunctions
    "and", "or", "but", "if", "then", "else", "when", "while", "because", "so", "though",
    "although", "unless", "until", "since", // Pronouns
    "i", "you", "he", "she", "it", "we", "they", "me", "him", "her", "us", "them", "my", "your",
    "his", "its", "our", "their", "mine", "yours", "this", "that", "these", "those",
    // be verbs
    "is", "am", "are", "was", "were", "be", "been", "being", // Auxiliary verbs
    "do", "does", "did", "have", "has", "had", "will", "would", "shall", "should", "may", "might",
    "must", "can", "could", // Interrogatives
    "why", "how", "what", "when", "where", "who", "which", "whom", "whose",
    // Other common words
    "not", "no", "yes", "all", "any", "some", "more", "most", "other", "such", "very", "just",
    "only", "own", "same", "than", "too", "also", "there", "here", "now", "then", "up", "down",
    "out",
];

fn chinese() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| CHINESE_STOPWORDS_RAW.iter().copied().collect())
}

fn english() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| ENGLISH_STOPWORDS_RAW.iter().copied().collect())
}

/// Check whether a term is a stopword. English match is case-insensitive.
pub fn is_stopword(word: &str) -> bool {
    if chinese().contains(word) {
        return true;
    }
    let lower = word.to_lowercase();
    english().contains(lower.as_str())
}

/// Drop stopwords from a token slice.
pub fn filter_stopwords<'a, I>(tokens: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    tokens
        .into_iter()
        .filter(|t| !is_stopword(t))
        .map(|t| t.to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_english_case_insensitive() {
        assert!(is_stopword("the"));
        assert!(is_stopword("THE"));
        assert!(is_stopword("The"));
    }

    #[test]
    fn detects_chinese() {
        assert!(is_stopword("的"));
        assert!(is_stopword("为什么"));
    }

    #[test]
    fn keeps_content_words() {
        assert!(!is_stopword("rust"));
        assert!(!is_stopword("飞书"));
    }

    #[test]
    fn filter_drops_stopwords() {
        let kept = filter_stopwords(["the", "quick", "brown", "fox"]);
        assert_eq!(kept, vec!["quick", "brown", "fox"]);
    }
}

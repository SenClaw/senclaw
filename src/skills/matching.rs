//! Which skill a prompt is about, by the words the skill's author wrote —
//! `triggers`, `when-to-use`, and (in [`Reading::Full`]) the example requests
//! quoted in the `description`. No model: this is the recall stage the
//! pre-turn skill router (`crate::decision::skill_route`) narrows down, and
//! the whole of the legacy matcher the engine still uses when routing is off.
//!
//! Measured on 224 installed skills and 32 real requests
//! (`plans/reports/research-260926-1032-pre-skill-jev-laya.md`): the legacy
//! reading is right 20/32 and force-loads a wrong skill 10 times; reading the
//! description's quoted examples too is right 23/32. SenClaw skills put their
//! example requests there ("Dùng khi người dùng hỏi "thời tiết hôm nay thế
//! nào", …"), and only 117 of 224 have a `when-to-use` at all.

use std::collections::HashSet;

use serde::Serialize;

/// A match below this score is not a match.
pub const THRESHOLD: u32 = 15;
/// A whole trigger, or a quoted example, found in the prompt.
const PHRASE_POINTS: u32 = 50;
/// Each word of a trigger or `when-to-use` that the prompt shares.
const WORD_POINTS: u32 = 5;

/// What a skill says about when to use it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkillCard {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub triggers: Vec<String>,
}

/// How much of a card is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// `when-to-use` + `triggers`, scored exactly as the engine always has.
    Legacy,
    /// Also the example requests quoted in the description, with function
    /// words ("không", "bạn", "the"…) no longer counting as overlap.
    Full,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Scored {
    pub name: String,
    pub score: u32,
    /// A whole trigger or quoted example was found — the strong signal, as
    /// opposed to words that merely overlap.
    pub phrase_hit: bool,
}

/// The pre-skill router's decision for one turn: load `name`'s instructions
/// (`force`) or only hint that it may help.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillRoute {
    pub name: String,
    pub force: bool,
}

/// Every quoted span (straight or curly quotes) of at least 3 bytes.
pub fn extract_quoted_phrases(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut inside = false;
    for ch in s.chars() {
        match ch {
            '"' | '\u{201C}' | '\u{201D}' => {
                if inside {
                    let trimmed = current.trim().to_string();
                    if trimmed.len() >= 3 {
                        out.push(trimmed);
                    }
                    current.clear();
                    inside = false;
                } else {
                    inside = true;
                }
            }
            _ if inside => current.push(ch),
            _ => {}
        }
    }
    out
}

/// Function words that match everything and mean nothing (Vietnamese first:
/// "không" and "bạn" are in half the skills' examples and most greetings).
const STOP_WORDS: &[&str] = &[
    "không", "của", "và", "là", "thì", "mà", "này", "kia", "đó", "được", "cho", "với", "các", "những", "một",
    "hai", "bạn", "tôi", "mình", "anh", "chị", "em", "họ", "đang", "đã", "sẽ", "rất", "quá", "lắm", "nhé",
    "nha", "vậy", "thế", "nào", "gì", "sao", "đâu", "khi", "nếu", "để", "vào", "ra", "lên", "xuống", "từ",
    "tới", "đến", "trong", "ngoài", "trên", "dưới", "sau", "trước", "giúp", "hãy", "xin", "làm", "cái",
    "việc", "người", "hôm", "nay", "còn", "cũng", "như", "theo", "bằng", "về", "hay", "hoặc", "the", "and",
    "for", "with", "this", "that", "these", "those", "are", "was", "been", "its", "your", "you", "our",
    "they", "them", "his", "her", "please", "can", "could", "would", "should", "will", "does", "did",
    "what", "which", "who", "how", "when", "where", "why", "all", "any", "some", "from", "into", "about",
    "than", "then", "just", "also", "not", "yes", "use", "user", "asks",
];

fn is_stop(w: &str) -> bool {
    STOP_WORDS.contains(&w)
}

/// The prompt, split the way the engine always has.
struct Prompt {
    lower: String,
    /// Words of ≥ 3 bytes (legacy) — combining acute/tilde kept inside words.
    words: HashSet<String>,
}

impl Prompt {
    fn new(prompt: &str) -> Prompt {
        let lower = prompt.to_lowercase();
        let words = lower
            .split(|c: char| !c.is_alphanumeric() && c != '\u{0301}' && c != '\u{0303}')
            .filter(|w| w.len() >= 3)
            .map(str::to_string)
            .collect();
        Prompt { lower, words }
    }

    /// Points for the words of `text` the prompt shares.
    fn overlap(&self, text_lower: &str, reading: Reading) -> u32 {
        text_lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| match reading {
                Reading::Legacy => w.len() >= 3,
                Reading::Full => w.chars().count() >= 3 && !is_stop(w),
            })
            .filter(|w| self.words.contains(*w))
            .count() as u32
            * WORD_POINTS
    }
}

/// Score one card.
fn score_card(p: &Prompt, card: &SkillCard, reading: Reading) -> Scored {
    let mut score = 0;
    let mut phrase_hit = false;
    if let Some(when) = card.when_to_use.as_deref() {
        let when_lower = when.to_lowercase();
        for quote in extract_quoted_phrases(&when_lower) {
            if p.lower.contains(&quote) {
                score += PHRASE_POINTS;
                phrase_hit = true;
            }
        }
        score += p.overlap(&when_lower, reading);
    }
    let mut phrases: Vec<String> = card
        .triggers
        .iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if reading == Reading::Full {
        phrases.extend(
            extract_quoted_phrases(&card.description.to_lowercase())
                .into_iter()
                .filter(|q| q.chars().count() >= 4),
        );
    }
    for phrase in phrases {
        // A whole phrase is a strong signal; otherwise fall back to the words
        // it shares, so multi-word triggers still count.
        if p.lower.contains(&phrase) {
            score += PHRASE_POINTS;
            phrase_hit = true;
        } else {
            score += p.overlap(&phrase, reading);
        }
    }
    Scored {
        name: card.name.clone(),
        score,
        phrase_hit,
    }
}

/// Every card scored, best first; ties keep the cards' order.
pub fn score(cards: &[SkillCard], prompt: &str, reading: Reading) -> Vec<Scored> {
    let p = Prompt::new(prompt);
    let mut out: Vec<Scored> = cards.iter().map(|c| score_card(&p, c, reading)).collect();
    out.sort_by(|a, b| b.score.cmp(&a.score));
    out
}

/// The best card at or above [`THRESHOLD`], if any.
pub fn best(scored: &[Scored]) -> Option<&Scored> {
    scored.first().filter(|s| s.score >= THRESHOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(name: &str, description: &str, when: Option<&str>, triggers: &[&str]) -> SkillCard {
        SkillCard {
            name: name.into(),
            description: description.into(),
            when_to_use: when.map(str::to_string),
            triggers: triggers.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn cards() -> Vec<SkillCard> {
        vec![
            card(
                "weather-xem",
                "Xem thời tiết qua app Thời tiết. Dùng khi người dùng hỏi \"thời tiết hôm nay thế nào\", \"mai có mưa không\".",
                None,
                &["thời tiết", "trời hôm nay thế nào"],
            ),
            card(
                "clock-timer",
                "Hẹn giờ và đếm ngược. Dùng khi người dùng muốn \"hẹn giờ X phút\", \"báo tôi sau Z phút\".",
                None,
                &["hẹn giờ", "đếm ngược"],
            ),
            card("ak:git", "Git operations.", Some("Use for \"commit\", \"push\" and branches"), &[]),
        ]
    }

    #[test]
    fn a_trigger_phrase_is_a_strong_match() {
        let s = score(&cards(), "đặt hẹn giờ 10 phút giúp tôi", Reading::Legacy);
        let top = best(&s).unwrap();
        assert_eq!((top.name.as_str(), top.phrase_hit), ("clock-timer", true));
        assert!(top.score >= PHRASE_POINTS);
    }

    #[test]
    fn only_the_full_reading_finds_the_descriptions_examples() {
        let prompt = "mai có mưa không nhỉ";
        let legacy = score(&cards(), prompt, Reading::Legacy);
        let full = score(&cards(), prompt, Reading::Full);
        assert!(!legacy[0].phrase_hit, "legacy never reads the description");
        assert_eq!(best(&full).map(|s| (s.name.as_str(), s.phrase_hit)), Some(("weather-xem", true)));
    }

    #[test]
    fn function_words_no_longer_make_a_greeting_a_match() {
        // Legacy: "không" and "bạn" overlap the weather examples' words.
        let c = vec![card("weather-xem", "", Some("thời tiết hôm nay không, bạn muốn biết không"), &[])];
        let greeting = "chào bạn, bạn khoẻ không?";
        assert!(best(&score(&c, greeting, Reading::Legacy)).is_some());
        assert!(best(&score(&c, greeting, Reading::Full)).is_none());
    }

    #[test]
    fn when_to_use_quotes_and_words_count_as_before() {
        let s = score(&cards(), "please commit and push this", Reading::Legacy);
        let top = best(&s).unwrap();
        assert_eq!(top.name, "ak:git");
        // Two quoted hits, plus "commit", "push" and "and" shared as words:
        // legacy counts function words too.
        assert_eq!(top.score, 2 * PHRASE_POINTS + 3 * WORD_POINTS);
    }

    #[test]
    fn quoted_phrases_accept_curly_quotes() {
        assert_eq!(
            extract_quoted_phrases("e.g. \u{201C}tìm giá vàng\u{201D} or \"screenshot\""),
            vec!["tìm giá vàng", "screenshot"]
        );
    }
}

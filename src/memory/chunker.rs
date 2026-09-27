//! Text chunker. Mirrors `src-old/memory/chunker.ts`.
//!
//! Chunk by lines with overlap; line numbers preserved (1-based, inclusive end).
//! Token counting is a lightweight estimate — no tiktoken.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    /// 1-based start line.
    pub start_line: usize,
    /// 1-based end line, inclusive.
    pub end_line: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct ChunkerOptions {
    pub chunk_size: usize,
    pub chunk_overlap: usize,
}

impl Default for ChunkerOptions {
    fn default() -> Self {
        Self {
            chunk_size: 400,
            chunk_overlap: 80,
        }
    }
}

fn is_cjk_for_estimate(c: char) -> bool {
    matches!(c,
        '\u{4e00}'..='\u{9fff}'   // Han
        | '\u{3040}'..='\u{309f}' // Hiragana
        | '\u{30a0}'..='\u{30ff}' // Katakana
        | '\u{ac00}'..='\u{d7af}' // Hangul
    )
}

/// Lightweight token estimate. CJK chars ≈ 1 token / 1.2 chars; other text
/// counts whitespace-separated words.
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk_chars = 0usize;
    let mut non_cjk_buf = String::with_capacity(text.len());
    for c in text.chars() {
        if is_cjk_for_estimate(c) {
            cjk_chars += 1;
            non_cjk_buf.push(' ');
        } else {
            non_cjk_buf.push(c);
        }
    }
    let mut tokens = 0usize;
    if cjk_chars > 0 {
        // ceil(cjk / 1.2)
        tokens += (cjk_chars * 10 + 11) / 12;
    }
    tokens += non_cjk_buf.split_whitespace().count();
    if tokens == 0 {
        0
    } else {
        tokens.max(1)
    }
}

/// Chunk text by line. Returned chunks omit the hash field — callers compute
/// the SHA-256 themselves once embedding-side concerns are wired up.
pub fn chunk_text(text: &str, options: ChunkerOptions) -> Vec<Chunk> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut start_idx = 0usize;

    while start_idx < lines.len() {
        let mut token_count = 0usize;
        let mut end_idx = start_idx;

        while end_idx < lines.len() {
            let line_tokens = estimate_tokens(lines[end_idx]);
            if token_count + line_tokens > options.chunk_size && end_idx > start_idx {
                break;
            }
            token_count += line_tokens;
            end_idx += 1;
        }

        let chunk_text = lines[start_idx..end_idx].join("\n");
        chunks.push(Chunk {
            text: chunk_text,
            start_line: start_idx + 1,
            end_line: end_idx,
        });

        if end_idx >= lines.len() {
            break;
        }

        let mut overlap_tokens = 0usize;
        let mut next_start = end_idx;
        while next_start > start_idx && overlap_tokens < options.chunk_overlap {
            next_start -= 1;
            overlap_tokens += estimate_tokens(lines[next_start]);
        }
        // Advance at least one line so a single oversized line cannot loop.
        start_idx = next_start.max(start_idx + 1);
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_english_words() {
        assert_eq!(estimate_tokens("hello world foo"), 3);
    }

    #[test]
    fn estimate_cjk_uses_ratio() {
        // 12 CJK chars → ceil(12/1.2) = 10
        assert_eq!(estimate_tokens("飞书同步飞书同步飞书同步"), 10);
    }

    #[test]
    fn empty_text_no_chunks() {
        assert!(chunk_text("", ChunkerOptions::default()).is_empty());
        assert!(chunk_text("   \n\n  ", ChunkerOptions::default()).is_empty());
    }

    #[test]
    fn small_text_single_chunk() {
        let out = chunk_text("hello world", ChunkerOptions::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_line, 1);
        assert_eq!(out[0].end_line, 1);
        assert_eq!(out[0].text, "hello world");
    }

    #[test]
    fn splits_when_over_chunk_size() {
        // 10 lines of 20 words each = 200 tokens; chunk_size=50 forces splits.
        let line: String = (0..20).map(|i| format!("w{i} ")).collect();
        let text = (0..10).map(|_| line.trim()).collect::<Vec<_>>().join("\n");
        let chunks = chunk_text(
            &text,
            ChunkerOptions {
                chunk_size: 50,
                chunk_overlap: 10,
            },
        );
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.start_line >= 1);
            assert!(c.end_line >= c.start_line);
        }
        // Last chunk must reach the final line.
        assert_eq!(chunks.last().unwrap().end_line, 10);
    }

    #[test]
    fn oversized_single_line_advances() {
        // One line larger than chunk_size — must still advance, not loop.
        let big: String = (0..1000).map(|i| format!("w{i} ")).collect();
        let text = format!("{big}\nshort line");
        let chunks = chunk_text(
            &text,
            ChunkerOptions {
                chunk_size: 10,
                chunk_overlap: 5,
            },
        );
        assert!(chunks.len() >= 2);
    }
}

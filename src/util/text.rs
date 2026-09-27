//! Text helpers.

/// Truncate `s` to at most `max_bytes` bytes, snapping DOWN to the nearest
/// UTF-8 char boundary. Returns the whole string if it already fits.
///
/// Slicing a `&str` with `&s[..n]` panics when `n` lands inside a multi-byte
/// codepoint — which happens constantly with non-ASCII text (Vietnamese,
/// Chinese, emoji, …) at a fixed byte budget. Every "truncate to N chars for a
/// preview/prompt" site must go through this instead of a raw byte slice.
pub fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_untouched_within_budget() {
        assert_eq!(truncate_on_char_boundary("hello", 10), "hello");
        assert_eq!(truncate_on_char_boundary("hello", 3), "hel");
    }

    #[test]
    fn never_panics_mid_codepoint_vietnamese() {
        // "chào bạn, hôm nay thế nào" — multi-byte chars at many offsets. A raw
        // `&s[..n]` would panic; the helper snaps back to a boundary.
        let s = "chào bạn, hôm nay thế nào";
        for n in 0..=s.len() + 5 {
            let out = truncate_on_char_boundary(s, n);
            assert!(s.starts_with(out), "output must be a valid prefix");
            assert!(out.len() <= n, "output must respect the byte budget");
        }
    }

    #[test]
    fn snaps_down_to_boundary() {
        // 'ớ' occupies 3 bytes; a budget landing inside it yields the prefix
        // before it, never a panic.
        let s = "ớ";
        assert_eq!(truncate_on_char_boundary(s, 1), "");
        assert_eq!(truncate_on_char_boundary(s, 2), "");
        assert_eq!(truncate_on_char_boundary(s, 3), "ớ");
    }
}

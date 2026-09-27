//! Utility functions for permission bridge — ID generation, formatting, truncation.

pub(crate) fn short_id() -> String {
    use rand::Rng;
    let bytes: [u8; 4] = rand::thread_rng().gen();
    hex::encode(bytes) // 8 chars
}

pub(crate) fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

/// Format sema-core content into a readable string.
///
/// - string → returned as-is
/// - object with `patch[].lines` (file write/edit diff) → extracted diff lines
/// - other objects → JSON pretty-printed
pub(crate) fn format_content(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }

    // Try extracting diff lines from patch structure
    if let Some(patch) = content.get("patch").and_then(|p| p.as_array()) {
        let mut lines: Vec<String> = Vec::new();
        for hunk in patch {
            if let Some(hunk_lines) = hunk.get("lines").and_then(|l| l.as_array()) {
                for line in hunk_lines {
                    if let Some(s) = line.as_str() {
                        lines.push(s.to_string());
                    }
                }
            }
        }
        if !lines.is_empty() {
            return lines.join("\n");
        }
    }

    serde_json::to_string_pretty(content).unwrap_or_default()
}

/// Truncate by **byte budget** `max_len` (same as before), but only at UTF-8 character
/// boundaries so Vietnamese / emoji never panic. Overflow line counts omitted **characters**
/// in the suffix.
pub(crate) fn truncate_content(content: &str, max_len: usize) -> String {
    if content.len() <= max_len {
        return content.to_string();
    }
    let mut end = max_len.min(content.len());
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = content[end..].chars().count();
    format!("{}\n...({omitted} chars omitted)", &content[..end])
}

//! Multi-format text extraction for cognitive file ingestion.
//!
//! Port of ai-agent-chatbot's `pkg/domain/services/knowledge/text_extract.go`
//! (Phase 1 — text formats only). Turns an uploaded file's raw bytes into
//! clean UTF-8 text ready to feed into [`crate::memory::cognitive::cognify`].
//!
//! Supported: `txt md markdown html htm json csv tsv yaml yml log xml`.
//! Binary office/media formats (pdf, docx, png, …) are rejected with a clear
//! error rather than producing garbage triplets. Detection order mirrors the
//! Go original: file extension → MIME content-type → byte sniff.

use anyhow::{bail, Result};

/// Hard cap on a single upload. Matches the source service's 10 MB limit.
pub const MAX_FILE_SIZE_BYTES: usize = 10 * 1024 * 1024;

/// Extract clean plain text from an uploaded file.
///
/// * `filename` — drives format detection by extension (case-insensitive).
/// * `content_type` — MIME fallback when the extension is missing/unknown.
/// * `data` — the raw file bytes (already buffered; ≤ [`MAX_FILE_SIZE_BYTES`]).
pub fn extract_text(filename: &str, content_type: &str, data: &[u8]) -> Result<String> {
    if data.len() > MAX_FILE_SIZE_BYTES {
        bail!(
            "file too large: {} bytes (max {} bytes / {} MB)",
            data.len(),
            MAX_FILE_SIZE_BYTES,
            MAX_FILE_SIZE_BYTES / (1024 * 1024)
        );
    }
    if data.is_empty() {
        return Ok(String::new());
    }

    match detect_format(filename, content_type, data) {
        Format::Html => {
            let decoded = String::from_utf8_lossy(data);
            Ok(collapse_whitespace(&clean_text(&strip_html(&decoded))))
        }
        Format::Text => {
            let decoded = String::from_utf8_lossy(data);
            Ok(clean_text(&decoded))
        }
        Format::Unsupported(label) => bail!(
            "unsupported file format `{label}` — only plain-text formats are accepted \
             (txt, md, html, json, csv, tsv, yaml, xml, log)"
        ),
    }
}

enum Format {
    Text,
    Html,
    Unsupported(String),
}

fn detect_format(filename: &str, content_type: &str, data: &[u8]) -> Format {
    // ── 1. Extension ──────────────────────────────────────────────────
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" | "xhtml" => return Format::Html,
        "txt" | "text" | "md" | "markdown" | "json" | "csv" | "tsv" | "log" | "yaml" | "yml"
        | "xml" | "ndjson" | "jsonl" => return Format::Text,
        // Well-known binary formats: fail loudly instead of sniffing.
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "zip" | "gz" | "tar" | "png"
        | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "mp3" | "mp4" | "wav" | "ogg"
        | "avi" | "mov" | "exe" | "bin" | "so" | "dylib" | "dll" => {
            return Format::Unsupported(ext)
        }
        _ => {}
    }

    // ── 2. MIME content-type ──────────────────────────────────────────
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("html") {
        return Format::Html;
    }
    if ct.starts_with("text/")
        || ct.contains("json")
        || ct.contains("csv")
        || ct.contains("yaml")
        || ct.contains("markdown")
        || ct.contains("xml")
    {
        return Format::Text;
    }

    // ── 3. Byte sniff ─────────────────────────────────────────────────
    // A NUL byte (or a high density of other control bytes) in the head is
    // a strong "this is binary" signal.
    if looks_like_text(data) {
        Format::Text
    } else {
        let label = if ext.is_empty() {
            if content_type.is_empty() {
                "binary".to_string()
            } else {
                content_type.to_string()
            }
        } else {
            ext
        };
        Format::Unsupported(label)
    }
}

/// Heuristic: the first 8 KB is valid-ish text with no NUL and few control
/// bytes. Cheap and good enough to gate accidental binary uploads.
fn looks_like_text(data: &[u8]) -> bool {
    let head = &data[..data.len().min(8192)];
    if head.contains(&0) {
        return false;
    }
    let suspicious = head
        .iter()
        .filter(|&&b| b < 0x09 || (b > 0x0d && b < 0x20))
        .count();
    // Allow up to ~5% odd control bytes before calling it binary.
    suspicious * 20 <= head.len()
}

/// Strip HTML to text: drop `<script>`/`<style>` blocks wholesale, replace
/// every remaining tag with a space (so adjacent words don't fuse), then
/// decode the handful of entities that matter for readability.
fn strip_html(input: &str) -> String {
    let bytes = input.as_bytes();
    // ASCII-lowercasing preserves byte length and char boundaries, so byte
    // offsets stay valid across both `input` and `lower`.
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if bytes[i] == b'<' {
            let rest = &lower[i..];
            if rest.starts_with("<script") {
                match rest.find("</script>") {
                    Some(end) => {
                        i += end + "</script>".len();
                        continue;
                    }
                    None => break,
                }
            }
            if rest.starts_with("<style") {
                match rest.find("</style>") {
                    Some(end) => {
                        i += end + "</style>".len();
                        continue;
                    }
                    None => break,
                }
            }
            match input[i..].find('>') {
                Some(end) => {
                    out.push(' '); // tags become word boundaries
                    i += end + 1;
                    continue;
                }
                None => break, // dangling '<' — stop, drop the remainder
            }
        }
        // SAFETY: i is always at a char boundary (we advance by full chars
        // or past an ASCII '>').
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    decode_entities(&out)
}

fn decode_entities(s: &str) -> String {
    // `&amp;` decoded last so we never synthesise a fresh entity.
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Strip C0/C1 control characters (except tab/newline), normalise line
/// endings to `\n`, drop the U+FFFD left by lossy decoding, and trim.
fn clean_text(s: &str) -> String {
    let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .chars()
        .filter(|&c| c == '\t' || c == '\n' || (!c.is_control() && c != '\u{FFFD}'))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Collapse runs of horizontal whitespace to a single space and runs of 2+
/// blank lines to one. Applied to HTML output (which is whitespace-noisy);
/// plain text/markdown is left structurally intact.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0u32;
    for raw_line in s.lines() {
        let mut line = String::with_capacity(raw_line.len());
        let mut prev_ws = false;
        for c in raw_line.chars() {
            if c == ' ' || c == '\t' {
                if !prev_ws {
                    line.push(' ');
                    prev_ws = true;
                }
            } else {
                line.push(c);
                prev_ws = false;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        let out = extract_text("note.txt", "text/plain", b"hello world\n").unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn markdown_structure_preserved() {
        let md = "# Title\n\n- a\n- b\n\n```\ncode  with  spaces\n```";
        let out = extract_text("readme.md", "", md.as_bytes()).unwrap();
        // Markdown is NOT whitespace-collapsed: inner spacing survives.
        assert!(out.contains("# Title"));
        assert!(out.contains("code  with  spaces"));
    }

    #[test]
    fn html_is_stripped_and_entities_decoded() {
        let html = r#"<html><head><style>.x{color:red}</style>
            <script>alert('x')</script></head>
            <body><h1>Ada &amp; Lovelace</h1><p>price &lt; 100k</p></body></html>"#;
        let out = extract_text("page.html", "text/html", html.as_bytes()).unwrap();
        assert!(out.contains("Ada & Lovelace"), "got: {out:?}");
        assert!(out.contains("price < 100k"), "got: {out:?}");
        // script/style content must be gone
        assert!(!out.contains("alert"), "got: {out:?}");
        assert!(!out.contains("color:red"), "got: {out:?}");
        // No leftover HTML tags. (A lone `<` from the decoded `&lt;` is
        // legitimate content, so we check for tag shapes, not the bare char.)
        assert!(!out.to_lowercase().contains("<body"), "got: {out:?}");
        assert!(!out.contains("<h1>"), "got: {out:?}");
        assert!(!out.contains("</"), "got: {out:?}");
    }

    #[test]
    fn detects_html_by_content_type_without_extension() {
        let out = extract_text("upload", "text/html; charset=utf-8", b"<b>hi</b>").unwrap();
        assert_eq!(out, "hi");
    }

    #[test]
    fn unsupported_binary_extension_errors() {
        let err = extract_text("doc.pdf", "application/pdf", b"%PDF-1.4 ...").unwrap_err();
        assert!(err.to_string().contains("unsupported"), "got: {err}");
    }

    #[test]
    fn nul_bytes_sniffed_as_binary() {
        let err = extract_text("mystery", "", b"\x00\x01\x02 binary \x00").unwrap_err();
        assert!(err.to_string().contains("unsupported"), "got: {err}");
    }

    #[test]
    fn oversize_is_rejected() {
        let big = vec![b'a'; MAX_FILE_SIZE_BYTES + 1];
        let err = extract_text("big.txt", "text/plain", &big).unwrap_err();
        assert!(err.to_string().contains("too large"), "got: {err}");
    }

    #[test]
    fn empty_yields_empty() {
        assert_eq!(extract_text("e.txt", "text/plain", b"").unwrap(), "");
    }

    #[test]
    fn control_chars_are_stripped() {
        let out = extract_text("c.txt", "text/plain", b"a\x07b\tc\n").unwrap();
        assert_eq!(out, "ab\tc");
    }

    #[test]
    fn utf8_multibyte_html_survives() {
        // Vietnamese text with a tag in the middle — byte-offset handling
        // must not split a multibyte char.
        let html = "<p>Giá cà phê &lt; 100k đồng</p>";
        let out = extract_text("vn.html", "text/html", html.as_bytes()).unwrap();
        assert!(out.contains("Giá cà phê < 100k đồng"), "got: {out:?}");
    }
}

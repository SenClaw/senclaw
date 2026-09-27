//! Non-image chat attachments: save to disk, pull out readable text, and fold
//! that text into the prompt.
//!
//! Images have their own route (vision blocks, else OCR) in
//! [`crate::agent::input_builder`]. Everything else lands here. Two things
//! always reach the model, in this order of usefulness:
//!
//! 1. the **on-disk path**, so the agent can Read/grep the whole file with its
//!    normal tools when the inlined preview is truncated or the format is one
//!    we can't parse; and
//! 2. an **inlined text preview**, so the common case (a short note, a CSV, a
//!    config file) needs no tool call at all.
//!
//! Extraction is best-effort by design: an unreadable file still yields a saved
//! path and an honest "could not extract" line rather than failing the turn.

use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD, Engine as _};

use crate::types::MessageAttachment;

/// Longest run of extracted text inlined per document.
///
/// Big enough for a whole short document, small enough that a 200-page PDF
/// can't eat the model's context. Past this the agent is told to read the file
/// from disk instead.
pub const MAX_INLINE_CHARS: usize = 20_000;

/// Refuse to decode an attachment larger than this. The base64 payload has
/// already crossed a WebSocket frame by the time we see it, so this is a
/// backstop against writing something absurd to disk, not a transport limit.
pub const MAX_DOC_BYTES: usize = 32 * 1024 * 1024;

/// One attached document as the model gets to see it.
#[derive(Debug, Clone)]
pub struct DocumentExtract {
    pub name: String,
    pub mime_type: String,
    /// Where the file was written, when saving succeeded. The agent reads more
    /// from here.
    pub path: Option<PathBuf>,
    /// Byte size of the decoded file, when known.
    pub bytes: Option<usize>,
    /// Extracted text, already truncated to [`MAX_INLINE_CHARS`].
    pub text: Option<String>,
    /// Why there is no text, when there isn't any.
    pub error: Option<String>,
    /// Whether [`Self::text`] was cut short.
    pub truncated: bool,
}

/// Decode an attachment's payload into raw bytes.
///
/// Handles `data:` URLs (what every composer sends) and absolute local paths
/// (what a channel adapter may hand over after downloading). Deliberately does
/// **not** fetch http(s): a URL in a chat message is attacker-controllable and
/// fetching it server-side is an SSRF the agent's own tools already gate.
pub fn decode_payload(data_url: &str) -> Result<Vec<u8>, String> {
    if let Some(rest) = data_url.strip_prefix("data:") {
        let Some(idx) = rest.find(";base64,") else {
            return Err("data URL is not base64-encoded".into());
        };
        let b64 = &rest[idx + ";base64,".len()..];
        let bytes = STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| format!("invalid base64: {e}"))?;
        if bytes.len() > MAX_DOC_BYTES {
            return Err(format!("file is {} bytes, over the limit", bytes.len()));
        }
        return Ok(bytes);
    }
    let path = data_url.strip_prefix("file://").unwrap_or(data_url);
    if path.starts_with('/') {
        let meta = std::fs::metadata(path).map_err(|e| format!("cannot stat file: {e}"))?;
        if meta.len() as usize > MAX_DOC_BYTES {
            return Err(format!("file is {} bytes, over the limit", meta.len()));
        }
        return std::fs::read(path).map_err(|e| format!("cannot read file: {e}"));
    }
    Err("unsupported attachment source".into())
}

/// Reduce a chat JID to something safe to use as one directory name.
///
/// JIDs carry `:` and `/` (`web:main`, `tg:123:group:-456`), either of which
/// would turn one attachment directory into a tree — or, with `..`, escape the
/// uploads root entirely.
pub fn sanitize_jid(jid: &str) -> String {
    let cleaned: String = jid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        "chat".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Strip a client-supplied filename down to a leaf name that cannot traverse.
///
/// The name comes straight off the wire, so `../../.ssh/authorized_keys` is a
/// thing a client can send; only the final component survives, and a name that
/// is empty or all dots is replaced.
pub fn sanitize_file_name(name: &str) -> String {
    let leaf = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('.');
    let cleaned: String = leaf
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned
    }
}

/// Write one attachment under `<uploads_dir>/<sanitized jid>/<stamp>-<name>`.
///
/// The timestamp prefix keeps two uploads of `report.pdf` from overwriting each
/// other, and keeps the directory listing chronological.
pub fn save_document(
    uploads_dir: &Path,
    jid: &str,
    name: &str,
    stamp: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    let dir = uploads_dir.join(sanitize_jid(jid));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{stamp}-{}", sanitize_file_name(name)));
    std::fs::write(&path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Pull readable text out of a document, by MIME then by extension.
///
/// Returns `Err` with a reason the model can be told verbatim when the format
/// carries no text we can reach.
pub fn extract_text(mime_type: &str, name: &str, bytes: &[u8]) -> Result<String, String> {
    let mime = mime_type.split(';').next().unwrap_or("").trim();
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if is_ooxml_word(mime, &ext) {
        return extract_docx(bytes);
    }
    if is_texty(mime, &ext) {
        return decode_text(bytes);
    }
    if mime == "application/pdf" || ext == "pdf" {
        return Err(
            "PDF text extraction is not built in — read the saved file with your own tools".into(),
        );
    }
    // Unknown type: if it decodes as UTF-8 and isn't mostly control bytes, it's
    // text under a MIME we didn't list. Better to show it than to refuse.
    if let Ok(text) = decode_text(bytes) {
        if looks_textual(&text) {
            return Ok(text);
        }
    }
    Err(format!(
        "no text extractor for `{}` — read the saved file with your own tools",
        if mime.is_empty() { &ext } else { mime }
    ))
}

fn is_texty(mime: &str, ext: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-yaml"
                | "application/yaml"
                | "application/toml"
                | "application/sql"
        )
        || matches!(
            ext,
            "txt"
                | "md"
                | "markdown"
                | "csv"
                | "tsv"
                | "json"
                | "jsonl"
                | "ndjson"
                | "xml"
                | "yaml"
                | "yml"
                | "toml"
                | "ini"
                | "conf"
                | "cfg"
                | "log"
                | "sql"
                | "rs"
                | "py"
                | "js"
                | "ts"
                | "tsx"
                | "jsx"
                | "go"
                | "java"
                | "c"
                | "h"
                | "cpp"
                | "hpp"
                | "sh"
                | "html"
                | "css"
                | "dart"
        )
}

fn is_ooxml_word(mime: &str, ext: &str) -> bool {
    mime == "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        || ext == "docx"
}

fn decode_text(bytes: &[u8]) -> Result<String, String> {
    // Drop a UTF-8 BOM — it otherwise shows up as a stray glyph on line 1.
    let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8(body.to_vec()).map_err(|_| "file is not valid UTF-8 text".to_string())
}

/// Whether decoded bytes read as text rather than as a binary blob that merely
/// happens to be valid UTF-8.
fn looks_textual(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    let sample: String = text.chars().take(4096).collect();
    let control = sample
        .chars()
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        .count();
    control * 100 < sample.chars().count()
}

/// Extract the visible text of a `.docx`.
///
/// A docx is a zip; `word/document.xml` holds the body, where each `<w:t>` is a
/// run of literal text and `<w:p>` ends a paragraph. Reading just those two
/// tags avoids pulling in a full XML parser and, more importantly, avoids
/// dumping style/numbering markup into the prompt.
fn extract_docx(bytes: &[u8]) -> Result<String, String> {
    let reader = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| format!("not a valid .docx: {e}"))?;
    let mut xml = String::new();
    {
        use std::io::Read;
        let mut entry = zip
            .by_name("word/document.xml")
            .map_err(|_| "docx has no word/document.xml".to_string())?;
        entry
            .read_to_string(&mut xml)
            .map_err(|e| format!("cannot read word/document.xml: {e}"))?;
    }
    Ok(docx_xml_to_text(&xml))
}

/// Turn WordprocessingML body XML into plain text.
fn docx_xml_to_text(xml: &str) -> String {
    let mut out = String::new();
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        let Some(gt_rel) = rest[lt..].find('>') else {
            break;
        };
        let tag = &rest[lt + 1..lt + gt_rel];
        let after = &rest[lt + gt_rel + 1..];
        // `</w:p>` is a closing tag, `<w:br/>` a self-closing one, `<w:t
        // xml:space="preserve">` an opening tag with attributes — peel all three
        // down to the bare element name.
        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .split([' ', '\t', '\n', '\r', '/'])
            .next()
            .unwrap_or("");
        match (closing, name) {
            // `w:t` is the only element carrying literal text.
            (false, "w:t") if !tag.ends_with('/') => {
                if let Some(end) = after.find("</w:t>") {
                    out.push_str(&unescape_xml(&after[..end]));
                }
            }
            // Paragraph and line breaks are the document's own structure —
            // without them the whole file collapses into one unreadable line.
            (true, "w:p") | (false, "w:br") | (false, "w:cr") => out.push('\n'),
            (false, "w:tab") => out.push('\t'),
            _ => {}
        }
        rest = after;
    }
    // Word emits an empty paragraph between blocks; collapse the runs of blank
    // lines that produces.
    let mut collapsed = String::with_capacity(out.len());
    let mut blank_run = 0usize;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        collapsed.push_str(line.trim_end());
        collapsed.push('\n');
    }
    collapsed.trim().to_string()
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Cut `text` to [`MAX_INLINE_CHARS`], returning `(text, truncated)`.
///
/// Slices on a char boundary — a byte-index cut through a Vietnamese diacritic
/// panics.
pub fn truncate_for_prompt(text: &str) -> (String, bool) {
    if text.chars().count() <= MAX_INLINE_CHARS {
        return (text.to_string(), false);
    }
    let cut: String = text.chars().take(MAX_INLINE_CHARS).collect();
    (cut, true)
}

/// Build the `DocumentExtract` for one attachment: save it, then read it.
pub fn process_document(
    uploads_dir: &Path,
    jid: &str,
    stamp: &str,
    att: &MessageAttachment,
) -> DocumentExtract {
    let name = att.display_name().to_string();
    let mut out = DocumentExtract {
        name: name.clone(),
        mime_type: att.mime_type.clone(),
        path: None,
        bytes: None,
        text: None,
        error: None,
        truncated: false,
    };

    let bytes = match decode_payload(&att.data_url) {
        Ok(b) => b,
        Err(e) => {
            out.error = Some(e);
            return out;
        }
    };
    out.bytes = Some(bytes.len());

    match save_document(uploads_dir, jid, &name, stamp, &bytes) {
        Ok(path) => out.path = Some(path),
        // A failed save is not fatal: the text extract below still works, the
        // model just can't be pointed at a file for the rest.
        Err(e) => tracing::warn!("[documents] could not save `{name}`: {e}"),
    }

    match extract_text(&att.mime_type, &name, &bytes) {
        Ok(text) if text.trim().is_empty() => {
            out.error = Some("the file contains no readable text".into());
        }
        Ok(text) => {
            let (cut, truncated) = truncate_for_prompt(&text);
            out.text = Some(cut);
            out.truncated = truncated;
        }
        Err(e) => out.error = Some(e),
    }
    out
}

/// Fold extracted documents into the prompt.
///
/// Each document is fenced and labelled with its name, type and saved path. The
/// path is stated even when extraction worked, because a truncated preview is
/// the common case for anything long and the agent needs somewhere to go for
/// the rest.
pub fn append_document_context(prompt: &str, docs: &[DocumentExtract]) -> String {
    if docs.is_empty() {
        return prompt.to_string();
    }
    let mut out = String::from(prompt);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "[attached-files: {}]\nThe user attached the following file(s) to this message. \
         Contents are inlined below where they could be read; anything marked truncated or \
         unreadable is still on disk at the stated path, which you can open with your own \
         file tools. Never invent contents for a file you could not read — say so instead.\n",
        docs.len()
    ));

    for (i, d) in docs.iter().enumerate() {
        out.push_str(&format!(
            "\n--- file {} of {}: {} ({}",
            i + 1,
            docs.len(),
            d.name,
            d.mime_type
        ));
        if let Some(b) = d.bytes {
            out.push_str(&format!(", {}", human_bytes(b)));
        }
        out.push_str(") ---\n");
        match &d.path {
            Some(p) => out.push_str(&format!("saved at: {}\n", p.display())),
            None => out.push_str("saved at: (could not be saved to disk)\n"),
        }
        match (&d.text, &d.error) {
            (Some(text), _) => {
                out.push_str("```\n");
                out.push_str(text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n");
                if d.truncated {
                    out.push_str(&format!(
                        "[truncated at {MAX_INLINE_CHARS} characters — read the saved file for the rest]\n"
                    ));
                }
            }
            (None, Some(err)) => out.push_str(&format!("[could not extract text: {err}]\n")),
            (None, None) => out.push_str("[no text extracted]\n"),
        }
    }
    out
}

fn human_bytes(n: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(name: &str, mime: &str, body: &[u8]) -> MessageAttachment {
        MessageAttachment {
            data_url: format!("data:{mime};base64,{}", STANDARD.encode(body)),
            mime_type: mime.into(),
            name: Some(name.into()),
        }
    }

    #[test]
    fn decodes_base64_data_urls() {
        let bytes = decode_payload(&format!(
            "data:text/plain;base64,{}",
            STANDARD.encode("xin chào")
        ))
        .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "xin chào");
    }

    #[test]
    fn refuses_non_base64_and_remote_sources() {
        assert!(decode_payload("data:text/plain,plain").is_err());
        // http(s) is deliberately not fetched — a message-supplied URL turned
        // into a server-side GET is an SSRF.
        assert!(decode_payload("https://example.com/a.pdf").is_err());
    }

    #[test]
    fn sanitize_jid_flattens_separators() {
        assert_eq!(sanitize_jid("web:main"), "web_main");
        assert_eq!(sanitize_jid("tg:123:group:-456"), "tg_123_group_-456");
        // Traversal can't survive into a directory name.
        assert!(!sanitize_jid("../../etc").contains('.'));
        assert_eq!(sanitize_jid(":::"), "chat");
    }

    #[test]
    fn sanitize_file_name_keeps_only_a_leaf() {
        assert_eq!(sanitize_file_name("report.pdf"), "report.pdf");
        assert_eq!(
            sanitize_file_name("../../.ssh/authorized_keys"),
            "authorized_keys"
        );
        assert_eq!(sanitize_file_name("..\\..\\win.ini"), "win.ini");
        assert_eq!(sanitize_file_name("   "), "attachment");
        assert_eq!(sanitize_file_name(".."), "attachment");
    }

    #[test]
    fn saves_under_the_jid_directory_without_escaping_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = save_document(
            dir.path(),
            "web:main",
            "../../escape.txt",
            "20260814-120000",
            b"hi",
        )
        .unwrap();
        assert!(path.starts_with(dir.path().join("web_main")));
        assert_eq!(std::fs::read(&path).unwrap(), b"hi");
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("escape.txt"));
    }

    #[test]
    fn extracts_plain_text_by_mime_and_by_extension() {
        assert_eq!(
            extract_text("text/plain", "a.txt", "dòng 1\ndòng 2".as_bytes()).unwrap(),
            "dòng 1\ndòng 2"
        );
        // Unlisted MIME, known extension.
        assert_eq!(
            extract_text("application/octet-stream", "notes.md", b"# title").unwrap(),
            "# title"
        );
        // Neither, but it decodes as text anyway.
        assert_eq!(
            extract_text("", "mystery", b"just words here").unwrap(),
            "just words here"
        );
    }

    #[test]
    fn strips_a_utf8_bom() {
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice("hello".as_bytes());
        assert_eq!(extract_text("text/plain", "a.txt", &body).unwrap(), "hello");
    }

    #[test]
    fn binary_input_is_reported_not_dumped() {
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x01];
        let err = extract_text("application/octet-stream", "blob.bin", &png).unwrap_err();
        assert!(err.contains("read the saved file"));

        // PDF is named explicitly so the message says what to do next.
        let err = extract_text("application/pdf", "a.pdf", b"%PDF-1.4").unwrap_err();
        assert!(err.contains("PDF"));
    }

    #[test]
    fn docx_xml_keeps_text_and_paragraph_breaks() {
        let xml = r#"<w:document><w:body>
            <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Tiêu đề</w:t></w:r></w:p>
            <w:p><w:r><w:t>Câu một.</w:t></w:r><w:r><w:t xml:space="preserve"> Câu hai &amp; ba.</w:t></w:r></w:p>
            </w:body></w:document>"#;
        let text = docx_xml_to_text(xml);
        assert_eq!(text, "Tiêu đề\nCâu một. Câu hai & ba.");
        // Style markup must not leak into the prompt.
        assert!(!text.contains("Heading1"));
    }

    #[test]
    fn truncation_cuts_on_a_char_boundary() {
        // Multi-byte throughout: a byte-index cut here would panic.
        let long: String = "à".repeat(MAX_INLINE_CHARS + 100);
        let (cut, truncated) = truncate_for_prompt(&long);
        assert!(truncated);
        assert_eq!(cut.chars().count(), MAX_INLINE_CHARS);

        let (same, truncated) = truncate_for_prompt("ngắn");
        assert!(!truncated);
        assert_eq!(same, "ngắn");
    }

    #[test]
    fn process_document_saves_and_extracts() {
        let dir = tempfile::tempdir().unwrap();
        let d = process_document(
            dir.path(),
            "web:main",
            "20260814-120000",
            &att("ghi-chu.txt", "text/plain", "nội dung".as_bytes()),
        );
        assert_eq!(d.text.as_deref(), Some("nội dung"));
        assert!(d.path.is_some_and(|p| p.exists()));
        assert_eq!(d.bytes, Some("nội dung".len()));
        assert!(d.error.is_none());
    }

    #[test]
    fn prompt_block_states_the_path_and_forbids_inventing_contents() {
        let dir = tempfile::tempdir().unwrap();
        let readable = process_document(
            dir.path(),
            "web:main",
            "s",
            &att("a.txt", "text/plain", b"hello"),
        );
        let unreadable = process_document(
            dir.path(),
            "web:main",
            "s",
            &att("b.pdf", "application/pdf", b"%PDF-1.4"),
        );
        let out = append_document_context("Tóm tắt giúp tôi", &[readable, unreadable]);

        assert!(out.starts_with("Tóm tắt giúp tôi"));
        assert!(out.contains("[attached-files: 2]"));
        assert!(out.contains("Never invent contents"));
        assert!(out.contains("hello"));
        assert!(out.contains("could not extract text"));
        // Both files must be locatable on disk, readable or not.
        assert_eq!(out.matches("saved at: ").count(), 2);
        assert!(!out.contains("(could not be saved to disk)"));
    }

    #[test]
    fn no_documents_leaves_the_prompt_alone() {
        assert_eq!(append_document_context("chào", &[]), "chào");
    }
}

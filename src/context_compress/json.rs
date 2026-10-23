//! JSON plumbing for the compressor.
//!
//! Everything here works on the **text**, not on a parsed `serde_json::Value`
//! that is printed again: a `Value` turns `12345678901234567890123` into a
//! float and `1e3` into `1000.0`, and an id that changed is worse than a token
//! saved. Whitespace is stripped outside strings and rows are copied by byte
//! range, so what survives is byte-for-byte what the tool produced.

use serde::de::IgnoredAny;
use serde_json::Value;

/// What an MCP `tools/call` result is, once printed.
pub enum Envelope {
    /// Not an MCP result (or one with fields we do not understand): treat the
    /// text as it is.
    NotOne,
    /// An MCP result that must be left exactly as it came — an `isError`
    /// answer, or one carrying anything but text.
    Leave,
    /// `{"content":[{"type":"text","text":…}…]}` and nothing else: the text
    /// blocks, joined by a newline.
    Text(String),
}

pub fn classify_envelope(text: &str) -> Envelope {
    let t = text.trim_start();
    if !t.starts_with('{') {
        return Envelope::NotOne;
    }
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(t) else {
        return Envelope::NotOne;
    };
    let Some(Value::Array(blocks)) = map.get("content") else {
        return Envelope::NotOne;
    };
    // Every block names a `type` — otherwise this is some record that happens
    // to have a `content` array (chat messages, say), which is data.
    if blocks.is_empty()
        || !blocks
            .iter()
            .all(|b| b.get("type").and_then(Value::as_str).is_some())
    {
        return Envelope::NotOne;
    }
    for (key, value) in &map {
        match key.as_str() {
            "content" => {}
            "isError" => {
                if value != &Value::Bool(false) {
                    return Envelope::Leave;
                }
            }
            _ => return Envelope::Leave,
        }
    }
    let mut parts = Vec::with_capacity(blocks.len());
    for block in blocks {
        let only_text = block
            .as_object()
            .is_some_and(|o| o.len() == 2 && o.get("type").and_then(Value::as_str) == Some("text"));
        match (only_text, block.get("text").and_then(Value::as_str)) {
            (true, Some(s)) => parts.push(s),
            _ => return Envelope::Leave,
        }
    }
    Envelope::Text(parts.join("\n"))
}

/// A complete JSON object or array, nothing after it.
pub fn is_json_container(text: &str) -> bool {
    let t = text.trim();
    (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<IgnoredAny>(t).is_ok()
}

/// Strip the whitespace outside strings. `json` must be valid JSON.
pub fn minify(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in json.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ' ' | '\n' | '\r' | '\t' => {}
            _ => out.push(c),
        }
    }
    out
}

/// One array found in a minified document: where its brackets are and where
/// each element starts and ends (`[start, end)`, no surrounding whitespace).
#[derive(Debug, Clone)]
pub struct ArrayInfo {
    pub open: usize,
    pub close: usize,
    pub elems: Vec<(usize, usize)>,
}

/// Every array of a **minified** valid document, inner arrays first.
pub fn scan_arrays(text: &str) -> Vec<ArrayInfo> {
    struct Frame {
        is_array: bool,
        open: usize,
        elem_start: usize,
        elems: Vec<(usize, usize)>,
    }
    let bytes = text.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'"' => break,
                        _ => {}
                    }
                    i += 1;
                }
            }
            b'[' | b'{' => stack.push(Frame {
                is_array: bytes[i] == b'[',
                open: i,
                elem_start: i + 1,
                elems: Vec::new(),
            }),
            b',' => {
                if let Some(f) = stack.last_mut().filter(|f| f.is_array) {
                    f.elems.push((f.elem_start, i));
                    f.elem_start = i + 1;
                }
            }
            b']' | b'}' => {
                if let Some(mut f) = stack.pop() {
                    if f.is_array {
                        if f.elem_start < i {
                            f.elems.push((f.elem_start, i));
                        }
                        found.push(ArrayInfo {
                            open: f.open,
                            close: i,
                            elems: f.elems,
                        });
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minify_keeps_every_byte_of_strings_and_numbers() {
        let src = "{\n  \"a\": \"x  y\\n \\\"q\\\" \\\\\",\n  \"id\": 12345678901234567890123,\n  \"f\": 1e3,\n  \"vi\": \"Xin chào  bạn\"\n}";
        let min = minify(src);
        assert_eq!(
            min,
            "{\"a\":\"x  y\\n \\\"q\\\" \\\\\",\"id\":12345678901234567890123,\"f\":1e3,\"vi\":\"Xin chào  bạn\"}"
        );
        assert!(is_json_container(&min));
    }

    #[test]
    fn a_text_only_mcp_result_is_unwrapped_and_joined() {
        let pretty = serde_json::to_string_pretty(&serde_json::json!({
            "content": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}],
            "isError": false
        }))
        .unwrap();
        match classify_envelope(&pretty) {
            Envelope::Text(t) => assert_eq!(t, "one\ntwo"),
            _ => panic!("expected the text blocks"),
        }
    }

    #[test]
    fn an_error_result_or_one_with_an_image_is_left_alone() {
        let err = r#"{"content":[{"type":"text","text":"boom"}],"isError":true}"#;
        assert!(matches!(classify_envelope(err), Envelope::Leave));
        let img = r#"{"content":[{"type":"text","text":"a"},{"type":"image","data":"AA==","mimeType":"image/png"}]}"#;
        assert!(matches!(classify_envelope(img), Envelope::Leave));
        let extra = r#"{"content":[{"type":"text","text":"a"}],"structuredContent":{"x":1}}"#;
        assert!(matches!(classify_envelope(extra), Envelope::Leave));
    }

    #[test]
    fn data_with_a_content_array_is_not_an_mcp_result() {
        let chat = r#"{"content":[{"role":"user","text":"hi"}],"id":3}"#;
        assert!(matches!(classify_envelope(chat), Envelope::NotOne));
        assert!(matches!(classify_envelope("[1,2,3]"), Envelope::NotOne));
        assert!(matches!(classify_envelope("plain words"), Envelope::NotOne));
    }

    #[test]
    fn scan_finds_nested_arrays_with_exact_element_ranges() {
        let doc = r#"{"a":[1,[2,3],"x,]y"],"b":[]}"#;
        let arrays = scan_arrays(doc);
        let outer = arrays.iter().find(|a| a.elems.len() == 3).unwrap();
        let pieces: Vec<&str> = outer.elems.iter().map(|&(s, e)| &doc[s..e]).collect();
        assert_eq!(pieces, ["1", "[2,3]", "\"x,]y\""]);
        assert_eq!(
            arrays.iter().filter(|a| a.elems.is_empty()).count(),
            1,
            "the empty array has no elements"
        );
        assert_eq!(arrays.len(), 3);
    }

    #[test]
    fn only_a_complete_document_is_json() {
        assert!(is_json_container("[1, 2]"));
        assert!(!is_json_container("[1, 2] trailing"));
        assert!(!is_json_container("[1, 2"));
        assert!(!is_json_container("42"));
    }
}

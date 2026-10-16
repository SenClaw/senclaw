//! Reading a dataset file someone hands over — JSONL, a JSON array, CSV or
//! TSV — into rows of JSON objects, before any mapping.
//!
//! Only UTF-8 is read. A CSV saved by Excel in a legacy code page fails with
//! a message that says so, rather than being guessed at: a wrong guess turns
//! every Vietnamese diacritic into a different letter and nothing reports it.
//! A UTF-8 byte-order mark is dropped.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Jsonl,
    Json,
    Csv,
    Tsv,
}

impl Format {
    /// From a file name or URL path; `None` when the extension says nothing.
    pub fn from_name(name: &str) -> Option<Format> {
        let path = name.split(['?', '#']).next().unwrap_or(name).to_ascii_lowercase();
        let ext = path.rsplit('.').next().unwrap_or_default();
        match ext {
            "jsonl" | "ndjson" => Some(Format::Jsonl),
            "json" => Some(Format::Json),
            "csv" => Some(Format::Csv),
            "tsv" | "tab" => Some(Format::Tsv),
            _ => None,
        }
    }

    /// Guess from the first non-blank character when the name did not say.
    pub fn sniff(text: &str) -> Format {
        let t = text.trim_start_matches('\u{feff}').trim_start();
        let first_line = t.lines().next().unwrap_or_default();
        if t.starts_with('[') {
            Format::Json
        } else if t.starts_with('{') {
            // One object per line, or one object holding the rows.
            if first_line.trim_end().ends_with('}') && t.lines().filter(|l| !l.trim().is_empty()).count() > 1 {
                Format::Jsonl
            } else {
                Format::Json
            }
        } else if first_line.contains('\t') && !first_line.contains(',') {
            Format::Tsv
        } else {
            Format::Csv
        }
    }
}

/// Bytes to text, refusing anything that is not UTF-8.
pub fn utf8(bytes: Vec<u8>) -> Result<String, String> {
    let s = String::from_utf8(bytes).map_err(|_| {
        "the file is not UTF-8 text — save it as UTF-8 (in Excel: Save As → CSV UTF-8) and import it again".to_string()
    })?;
    Ok(s.trim_start_matches('\u{feff}').to_string())
}

/// The rows of `text`: at most `keep` of them returned, all of them counted.
/// A scalar row (a bare string in a JSON array or a JSONL line) becomes
/// `{"text": …}`, the shape seed files use.
pub fn parse(text: &str, format: Format, keep: usize) -> Result<(Vec<Value>, usize), String> {
    let text = text.trim_start_matches('\u{feff}');
    match format {
        Format::Jsonl => parse_jsonl(text, keep),
        Format::Json => parse_json(text, keep),
        Format::Csv => parse_delimited(text, b',', keep),
        Format::Tsv => parse_delimited(text, b'\t', keep),
    }
}

fn as_row(v: Value) -> Value {
    match v {
        Value::Object(_) => v,
        other => {
            let mut m = Map::new();
            m.insert("text".into(), other);
            Value::Object(m)
        }
    }
}

fn parse_jsonl(text: &str, keep: usize) -> Result<(Vec<Value>, usize), String> {
    let mut rows = Vec::new();
    let mut total = 0;
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        if rows.len() < keep {
            rows.push(as_row(v));
        }
        total += 1;
    }
    Ok((rows, total))
}

/// Keys a JSON document commonly keeps its rows under.
const ROW_KEYS: [&str; 5] = ["data", "rows", "records", "examples", "items"];

fn parse_json(text: &str, keep: usize) -> Result<(Vec<Value>, usize), String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let list = match v {
        Value::Array(a) => a,
        Value::Object(mut o) => match ROW_KEYS.iter().find_map(|k| match o.remove(*k) {
            Some(Value::Array(a)) => Some(a),
            _ => None,
        }) {
            Some(a) => a,
            None => return Err(format!("a JSON object needs its rows in an array under one of: {}", ROW_KEYS.join(", "))),
        },
        _ => return Err("a JSON file must hold an array of rows".into()),
    };
    let total = list.len();
    Ok((list.into_iter().take(keep).map(as_row).collect(), total))
}

/// RFC 4180: quoted fields may hold the delimiter, newlines and `""`. The
/// first record is the header; a blank or repeated column name is renamed
/// (`column_3`, `name_2`) so no value is silently dropped.
fn parse_delimited(text: &str, delim: u8, keep: usize) -> Result<(Vec<Value>, usize), String> {
    let mut records = Records { bytes: text.as_bytes(), pos: 0, delim };
    let Some(header) = records.next_record()? else { return Ok((Vec::new(), 0)) };
    let header = unique_names(header);
    let mut rows = Vec::new();
    let mut total = 0;
    while let Some(fields) = records.next_record()? {
        if fields.iter().all(|f| f.trim().is_empty()) {
            continue;
        }
        if rows.len() < keep {
            let mut m = Map::new();
            for (i, f) in fields.into_iter().enumerate() {
                let key = header.get(i).cloned().unwrap_or_else(|| format!("column_{}", i + 1));
                m.insert(key, Value::String(f));
            }
            rows.push(Value::Object(m));
        }
        total += 1;
    }
    Ok((rows, total))
}

fn unique_names(header: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(header.len());
    for (i, h) in header.into_iter().enumerate() {
        let base = if h.trim().is_empty() { format!("column_{}", i + 1) } else { h.trim().to_string() };
        let mut name = base.clone();
        let mut n = 2;
        while out.contains(&name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        out.push(name);
    }
    out
}

struct Records<'a> {
    bytes: &'a [u8],
    pos: usize,
    delim: u8,
}

impl Records<'_> {
    fn next_record(&mut self) -> Result<Option<Vec<String>>, String> {
        if self.pos >= self.bytes.len() {
            return Ok(None);
        }
        let mut fields = Vec::new();
        let mut field: Vec<u8> = Vec::new();
        let mut quoted = false;
        let start = self.pos;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            self.pos += 1;
            if quoted {
                if b == b'"' {
                    if self.bytes.get(self.pos) == Some(&b'"') {
                        field.push(b'"');
                        self.pos += 1;
                    } else {
                        quoted = false;
                    }
                } else {
                    field.push(b);
                }
                continue;
            }
            match b {
                b'"' if field.is_empty() => quoted = true,
                b'\r' if self.bytes.get(self.pos) == Some(&b'\n') => {}
                b'\n' | b'\r' => {
                    fields.push(take(&mut field));
                    return Ok(Some(fields));
                }
                _ if b == self.delim => fields.push(take(&mut field)),
                _ => field.push(b),
            }
        }
        if quoted {
            let line = self.bytes[..start].iter().filter(|b| **b == b'\n').count() + 1;
            return Err(format!("a quoted field opened on line {line} is never closed"));
        }
        fields.push(take(&mut field));
        Ok(Some(fields))
    }
}

fn take(field: &mut Vec<u8>) -> String {
    // The input is a &str, and a field only ever splits at an ASCII byte, so
    // every field is valid UTF-8.
    String::from_utf8(std::mem::take(field)).unwrap_or_default()
}

/// The header of a CSV / TSV text, named the way [`parse`] names its keys.
/// A JSON object does not keep its key order, so this is how the preview
/// shows a spreadsheet's columns in the order the file has them.
pub fn header(text: &str, format: Format) -> Option<Vec<String>> {
    let delim = match format {
        Format::Csv => b',',
        Format::Tsv => b'\t',
        _ => return None,
    };
    let text = text.trim_start_matches('\u{feff}');
    let mut records = Records { bytes: text.as_bytes(), pos: 0, delim };
    records.next_record().ok().flatten().map(unique_names)
}

/// `columns` ordered by where their first segment sits in `top` (a file's
/// header, a dataset's feature list); the rest keep their order after them.
pub fn ordered(mut columns: Vec<String>, top: &[String]) -> Vec<String> {
    let rank = |c: &String| {
        let first = c.split('.').next().unwrap_or(c);
        top.iter().position(|t| t == c || t == first).unwrap_or(usize::MAX)
    };
    columns.sort_by_key(rank);
    columns
}

/// Column paths of a row for mapping: its keys, one level into objects
/// (`answers.text`), with `.0` added where the value is a list
/// (`answers.text.0`).
pub fn columns(rows: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: String| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    for row in rows.iter().take(20) {
        let Some(o) = row.as_object() else { continue };
        for (k, v) in o {
            match v {
                Value::Object(inner) => {
                    for (k2, v2) in inner {
                        push(if v2.is_array() { format!("{k}.{k2}.0") } else { format!("{k}.{k2}") });
                    }
                }
                Value::Array(_) => push(format!("{k}.0")),
                _ => push(k.clone()),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn csv_handles_quotes_newlines_crlf_and_bom() {
        let text = "\u{feff}text,label\r\n\"đơn hàng, của tôi\",tra_don\r\n\"dòng\nhai\",\"nói \"\"chào\"\"\"\r\n\r\n";
        let (rows, total) = parse(text, Format::Csv, 10).unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows[0], json!({"text": "đơn hàng, của tôi", "label": "tra_don"}));
        assert_eq!(rows[1], json!({"text": "dòng\nhai", "label": "nói \"chào\""}));
    }

    #[test]
    fn csv_names_blank_and_repeated_columns_and_counts_past_keep() {
        let (rows, total) = parse("a,,a\n1,2,3,4\n5,6,7\n", Format::Csv, 1).unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows, vec![json!({"a": "1", "column_2": "2", "a_2": "3", "column_4": "4"})]);
        assert!(parse("a\n\"open\n", Format::Csv, 5).unwrap_err().contains("never closed"));
    }

    #[test]
    fn tsv_jsonl_and_json_read_into_objects() {
        let (rows, _) = parse("q\ta\nhai cộng hai\t4\n", Format::Tsv, 5).unwrap();
        assert_eq!(rows, vec![json!({"q": "hai cộng hai", "a": "4"})]);
        let (rows, total) = parse("{\"x\":1}\n\n\"bare\"\n", Format::Jsonl, 5).unwrap();
        assert_eq!((rows, total), (vec![json!({"x": 1}), json!({"text": "bare"})], 2));
        let (rows, _) = parse(r#"{"data": [{"x": 1}]}"#, Format::Json, 5).unwrap();
        assert_eq!(rows, vec![json!({"x": 1})]);
        assert!(parse(r#"{"x": 1}"#, Format::Json, 5).unwrap_err().contains("rows in an array"));
        assert!(parse("{\"x\":1}\nnope\n", Format::Jsonl, 5).unwrap_err().starts_with("line 2"));
    }

    #[test]
    fn format_comes_from_the_name_else_the_content() {
        assert_eq!(Format::from_name("https://x.org/a/b.JSONL?download=1"), Some(Format::Jsonl));
        assert_eq!(Format::from_name("data.txt"), None);
        assert_eq!(Format::sniff("  [1,2]"), Format::Json);
        assert_eq!(Format::sniff("{\"a\":1}\n{\"a\":2}\n"), Format::Jsonl);
        assert_eq!(Format::sniff("{\n \"rows\": []\n}"), Format::Json);
        assert_eq!(Format::sniff("a\tb\n1\t2"), Format::Tsv);
        assert_eq!(Format::sniff("a,b\n1,2"), Format::Csv);
    }

    #[test]
    fn only_utf8_is_read() {
        assert_eq!(utf8(b"\xef\xbb\xbfabc".to_vec()).unwrap(), "abc");
        // "Việt" in Windows-1258 is not valid UTF-8.
        assert!(utf8(vec![0x56, 0x69, 0xea, 0xf2, 0x74]).unwrap_err().contains("UTF-8"));
    }

    #[test]
    fn columns_reach_one_level_into_objects_and_lists_in_the_sources_order() {
        let rows = vec![json!({"id": "1", "answers": {"text": ["x"], "answer_start": [3]}, "tags": ["a"], "n": {"k": 1}})];
        let mut cols = columns(&rows);
        cols.sort();
        assert_eq!(cols, vec!["answers.answer_start.0", "answers.text.0", "id", "n.k", "tags.0"]);
        let top: Vec<String> = ["id", "tags", "answers", "n"].iter().map(|s| s.to_string()).collect();
        let cols = ordered(columns(&rows), &top);
        assert_eq!(&cols[..2], ["id", "tags.0"]);
        assert_eq!(cols.last().unwrap(), "n.k");
        assert_eq!(header("noi_dung,loai,\n1,2,3\n", Format::Csv).unwrap(), ["noi_dung", "loai", "column_3"]);
        assert_eq!(header("[]", Format::Json), None);
    }
}

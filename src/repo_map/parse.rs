//! One file → its definitions and references, via the grammar's `tags.scm`.

use tree_sitter::{Parser, QueryCursor, StreamingIterator};

use super::lang::{compiled, spec_for_path};
use super::{Def, Reference};

/// Longest signature line kept in the map; longer ones are cut so a single
/// generated declaration cannot eat the whole token budget.
const MAX_SIG_CHARS: usize = 140;

/// Files above this size are not parsed: generated bundles and data blobs
/// dominate them, and a tags pass over them is slow for no map value.
pub const MAX_PARSE_BYTES: u64 = 1024 * 1024;

pub struct Parsed {
    pub defs: Vec<Def>,
    pub refs: Vec<Reference>,
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim_end();
    if line.chars().count() > MAX_SIG_CHARS {
        let cut: String = line.chars().take(MAX_SIG_CHARS).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

/// Parse `source` as `path`'s language. `None` when the language is not
/// supported or the grammar failed to load.
pub fn parse_file(path: &str, source: &str) -> Option<Parsed> {
    let spec = spec_for_path(path)?;
    let compiled = compiled(spec)?;
    let mut parser = Parser::new();
    parser.set_language(&compiled.language).ok()?;
    let tree = parser.parse(source, None)?;
    let names = compiled.query.capture_names();
    let bytes = source.as_bytes();

    let mut defs = Vec::new();
    let mut refs = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&compiled.query, tree.root_node(), bytes);
    while let Some(m) = matches.next() {
        let mut name: Option<String> = None;
        let mut tag: Option<(&str, tree_sitter::Node)> = None;
        for cap in m.captures {
            let cname = names[cap.index as usize];
            if cname == "name" {
                if let Ok(t) = cap.node.utf8_text(bytes) {
                    name = Some(t.to_string());
                }
            } else if let Some(kind) = cname.strip_prefix("definition.") {
                tag = Some((kind, cap.node));
            } else if let Some(kind) = cname.strip_prefix("reference.") {
                tag = Some((kind, cap.node));
            }
        }
        let (Some(name), Some((kind, node))) = (name, tag) else { continue };
        if name.is_empty() {
            continue;
        }
        let cname_full = m
            .captures
            .iter()
            .map(|c| names[c.index as usize])
            .find(|n| n.starts_with("definition.") || n.starts_with("reference."))
            .unwrap_or("");
        let line = node.start_position().row as u32 + 1;
        if cname_full.starts_with("definition.") {
            let sig = node.utf8_text(bytes).map(first_line).unwrap_or_default();
            defs.push(Def {
                name,
                kind: kind.to_string(),
                line,
                end_line: node.end_position().row as u32 + 1,
                sig,
            });
        } else {
            refs.push(Reference { name, line });
        }
    }
    // A definition can be matched by two patterns (e.g. `function` and
    // `method` for the same node); keep the first per (name, line).
    defs.sort_by(|a, b| (a.line, &a.name).cmp(&(b.line, &b.name)));
    defs.dedup_by(|a, b| a.line == b.line && a.name == b.name);
    refs.sort_by(|a, b| (a.line, &a.name).cmp(&(b.line, &b.name)));
    refs.dedup_by(|a, b| a.line == b.line && a.name == b.name);
    Some(Parsed { defs, refs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_definitions_and_references() {
        let src = "pub struct Foo;\nimpl Foo {\n    pub fn new() -> Self { Foo }\n}\nfn main() {\n    let f = Foo::new();\n    helper(f);\n}\nfn helper(_f: Foo) {}\n";
        let p = parse_file("a.rs", src).unwrap();
        let names: Vec<&str> = p.defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"new"));
        assert!(names.contains(&"main"));
        assert!(names.contains(&"helper"));
        let foo = p.defs.iter().find(|d| d.name == "Foo").unwrap();
        assert_eq!(foo.kind, "class");
        assert_eq!(foo.line, 1);
        assert!(foo.sig.starts_with("pub struct Foo"));
        let helper_ref = p.refs.iter().find(|r| r.name == "helper").expect("call reference");
        assert_eq!(helper_ref.line, 7);
    }

    #[test]
    fn python_and_typescript_parse() {
        let py = "class A:\n    def m(self):\n        return go()\n\ndef go():\n    pass\n";
        let p = parse_file("x.py", py).unwrap();
        assert!(p.defs.iter().any(|d| d.name == "A" && d.kind == "class"));
        assert!(p.defs.iter().any(|d| d.name == "go"));
        let ts = "export function f(a: number): number { return g(a); }\nfunction g(x: number) { return x; }\nexport class K {}\n";
        let p = parse_file("y.ts", ts).unwrap();
        assert!(p.defs.iter().any(|d| d.name == "f"));
        assert!(p.defs.iter().any(|d| d.name == "K"));
        assert!(p.refs.iter().any(|r| r.name == "g"));
    }

    #[test]
    fn unsupported_extension_is_none() {
        assert!(parse_file("README.md", "# hi").is_none());
    }
}

//! Did this edit leave the file unparseable?
//!
//! SWE-agent's linter-guarded edit is the best-measured guard in the field
//! (18.0 % vs 15.0 % solved without it) and the reason is not the lint: after
//! one failed edit a model's next edit succeeds 57 % of the time against
//! 90 % for the first, because it now edits a broken file. Telling it at once,
//! in the edit's own result, with the line, is what stops that cascade.
//!
//! This is a parse check, not a type check, so it needs no server and runs in
//! microseconds (the LSP report, when a server exists, comes in addition). It
//! reports **only errors the edit introduced**: a file that was already
//! broken, or a construct the bundled grammar is too old to know, produces
//! the same count before and after and says nothing.

use tree_sitter::{Node, Parser};

use super::lang::{compiled, spec_for_path};

/// Files above this are not parsed; the same bound the repo map uses.
const MAX_BYTES: usize = 1024 * 1024;

/// Parse errors found in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxErrors {
    pub count: usize,
    /// 1-based line of the first one.
    pub first_line: usize,
    /// Trimmed source of the line it is on.
    pub snippet: String,
    /// `ERROR` (tokens the grammar could not place) or `MISSING ;` (a token it expected).
    pub kind: String,
}

/// Count the syntax errors in `source`, read as `path`'s language. `None`
/// when the language has no grammar here or the file is too large.
pub fn errors_in(path: &str, source: &str) -> Option<SyntaxErrors> {
    if source.len() > MAX_BYTES {
        return None;
    }
    let spec = spec_for_path(path)?;
    let compiled = compiled(spec)?;
    let mut parser = Parser::new();
    parser.set_language(&compiled.language).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if !root.has_error() {
        return Some(SyntaxErrors { count: 0, first_line: 0, snippet: String::new(), kind: String::new() });
    }
    let mut count = 0usize;
    let mut first: Option<(usize, String)> = None;
    walk(root, &mut |n| {
        let kind = if n.is_missing() {
            format!("MISSING {}", n.kind())
        } else if n.is_error() {
            "ERROR".to_string()
        } else {
            return;
        };
        count += 1;
        if first.is_none() {
            first = Some((n.start_position().row + 1, kind));
        }
    });
    let (line, kind) = first.unwrap_or((1, "ERROR".into()));
    let snippet = source.lines().nth(line - 1).unwrap_or("").trim().chars().take(160).collect();
    Some(SyntaxErrors { count, first_line: line, snippet, kind })
}

fn walk(node: Node<'_>, f: &mut dyn FnMut(Node<'_>)) {
    // An ERROR node already stands for everything under it; do not count its children.
    if node.is_error() || node.is_missing() {
        f(node);
        return;
    }
    if !node.has_error() {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, f);
    }
}

/// What to tell the model after an edit changed `before` into `after`
/// (`before` is `None` for a file the edit created). `None` when the edit
/// introduced no syntax error — or the language is not one we can parse.
pub fn introduced_by_edit(path: &str, before: Option<&str>, after: &str) -> Option<String> {
    let now = errors_in(path, after)?;
    if now.count == 0 {
        return None;
    }
    let was = before.and_then(|b| errors_in(path, b)).map_or(0, |e| e.count);
    if now.count <= was {
        return None;
    }
    Some(format!(
        "\n\nSyntax check: this edit leaves {} syntax error{} in {path} (first: {} near line {}: `{}`). The file is saved as written — fix it now, \
         before building on it; Read the lines around {} if the cause is not obvious.",
        now.count - was,
        if now.count - was == 1 { "" } else { "s" },
        now.kind,
        now.first_line,
        now.snippet,
        now.first_line,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_code_has_no_errors() {
        let e = errors_in("a.rs", "fn main() { let x = 1; }\n").unwrap();
        assert_eq!(e.count, 0);
        assert!(errors_in("a.py", "def f():\n    return 1\n").unwrap().count == 0);
        assert!(errors_in("a.ts", "export const f = (a: number): number => a + 1;\n").unwrap().count == 0);
    }

    #[test]
    fn a_broken_file_reports_where() {
        let e = errors_in("a.rs", "fn main() {\n    let x = ;\n}\n").unwrap();
        assert!(e.count >= 1);
        assert_eq!(e.first_line, 2, "{e:?}");
        assert!(e.snippet.contains("let x"));

        let py = errors_in("a.py", "def f(:\n    pass\n").unwrap();
        assert!(py.count >= 1);
    }

    #[test]
    fn an_unknown_language_or_a_huge_file_is_skipped() {
        assert_eq!(errors_in("notes.md", "fn ((("), None);
        assert_eq!(errors_in("a.rs", &"x".repeat(MAX_BYTES + 1)), None);
    }

    #[test]
    fn only_what_the_edit_introduced_is_reported() {
        let ok = "fn main() {\n    let x = 1;\n}\n";
        let broken = "fn main() {\n    let x = ;\n}\n";
        let note = introduced_by_edit("a.rs", Some(ok), broken).expect("introduced");
        assert!(note.contains("Syntax check") && note.contains("line 2"), "{note}");

        // Already broken before: the edit did not cause it.
        assert_eq!(introduced_by_edit("a.rs", Some(broken), broken), None);
        // Fixing it is silent.
        assert_eq!(introduced_by_edit("a.rs", Some(broken), ok), None);
        // A new file that is broken counts in full.
        assert!(introduced_by_edit("a.rs", None, broken).is_some());
        // A new, valid file is silent.
        assert_eq!(introduced_by_edit("a.rs", None, ok), None);
        // Languages without a grammar are never judged.
        assert_eq!(introduced_by_edit("a.toml", Some("a = 1"), "a = = ="), None);
    }
}

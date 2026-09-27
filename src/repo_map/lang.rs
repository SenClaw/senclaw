//! Language table: file extension → tree-sitter grammar + its bundled
//! `tags.scm`. Only grammars that ship a tags query are listed, because the
//! tags query is what names definitions and references; a grammar without one
//! (bash, haskell) would parse for nothing.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tree_sitter::{Language, Query};

#[derive(Clone, Copy)]
pub struct LangSpec {
    pub id: &'static str,
    language: fn() -> Language,
    /// Tags queries, concatenated at compile time. TypeScript's own query
    /// only names signatures and abstract members; the concrete
    /// `function_declaration` / `class_declaration` / call patterns come from
    /// the JavaScript query it extends, so both are loaded (Aider does the
    /// same).
    tags: &'static [&'static str],
}

/// Patterns the upstream Rust query lacks and a repo map needs: calls through
/// a path (`util::helper()`), and type mentions, which are how one file
/// depends on another's structs and traits.
const RUST_EXTRA_TAGS: &str = "
(call_expression
    function: (scoped_identifier
        name: (identifier) @name)) @reference.call

(generic_function
    function: (identifier) @name) @reference.call

(type_identifier) @name @reference.type
";

fn rust() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}
fn python() -> Language {
    tree_sitter_python::LANGUAGE.into()
}
fn typescript() -> Language {
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
}
fn tsx() -> Language {
    tree_sitter_typescript::LANGUAGE_TSX.into()
}
fn javascript() -> Language {
    tree_sitter_javascript::LANGUAGE.into()
}
fn go() -> Language {
    tree_sitter_go::LANGUAGE.into()
}
fn java() -> Language {
    tree_sitter_java::LANGUAGE.into()
}
fn c() -> Language {
    tree_sitter_c::LANGUAGE.into()
}
fn cpp() -> Language {
    tree_sitter_cpp::LANGUAGE.into()
}
fn csharp() -> Language {
    tree_sitter_c_sharp::LANGUAGE.into()
}

const RUST: LangSpec = LangSpec { id: "rust", language: rust, tags: &[tree_sitter_rust::TAGS_QUERY, RUST_EXTRA_TAGS] };
const PYTHON: LangSpec = LangSpec { id: "python", language: python, tags: &[tree_sitter_python::TAGS_QUERY] };
const TYPESCRIPT: LangSpec = LangSpec { id: "typescript", language: typescript, tags: &[tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY] };
const TSX: LangSpec = LangSpec { id: "tsx", language: tsx, tags: &[tree_sitter_javascript::TAGS_QUERY, tree_sitter_typescript::TAGS_QUERY] };
const JAVASCRIPT: LangSpec = LangSpec { id: "javascript", language: javascript, tags: &[tree_sitter_javascript::TAGS_QUERY] };
const GO: LangSpec = LangSpec { id: "go", language: go, tags: &[tree_sitter_go::TAGS_QUERY] };
const JAVA: LangSpec = LangSpec { id: "java", language: java, tags: &[tree_sitter_java::TAGS_QUERY] };
const C: LangSpec = LangSpec { id: "c", language: c, tags: &[tree_sitter_c::TAGS_QUERY] };
const CPP: LangSpec = LangSpec { id: "cpp", language: cpp, tags: &[tree_sitter_cpp::TAGS_QUERY] };
const CSHARP: LangSpec = LangSpec { id: "csharp", language: csharp, tags: &[tree_sitter_c_sharp::TAGS_QUERY] };

/// Grammar for a file, by extension.
pub fn spec_for_path(path: &str) -> Option<LangSpec> {
    let ext = path.rsplit('.').next()?;
    Some(match ext {
        "rs" => RUST,
        "py" | "pyi" => PYTHON,
        "ts" | "mts" | "cts" => TYPESCRIPT,
        "tsx" => TSX,
        "js" | "mjs" | "cjs" | "jsx" => JAVASCRIPT,
        "go" => GO,
        "java" => JAVA,
        "c" | "h" => C,
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => CPP,
        "cs" => CSHARP,
        _ => return None,
    })
}

/// Compiled grammar + tags query, one per language for the process.
pub struct Compiled {
    pub language: Language,
    pub query: Query,
}

static COMPILED: OnceLock<Mutex<HashMap<&'static str, Option<std::sync::Arc<Compiled>>>>> =
    OnceLock::new();

/// Compile lazily and cache. A grammar whose tags query fails to compile is
/// remembered as `None` so the failure is logged once, not per file.
pub fn compiled(spec: LangSpec) -> Option<std::sync::Arc<Compiled>> {
    let cache = COMPILED.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(spec.id) {
        return hit.clone();
    }
    let language = (spec.language)();
    let source = spec.tags.join("\n");
    let built = match Query::new(&language, &source) {
        Ok(query) => Some(std::sync::Arc::new(Compiled { language, query })),
        Err(e) => {
            tracing::warn!(lang = spec.id, error = %e, "[RepoMap] tags query failed to compile; language skipped");
            None
        }
    };
    cache.lock().unwrap().insert(spec.id, built.clone());
    built
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_grammar_compiles_its_tags_query() {
        for spec in [RUST, PYTHON, TYPESCRIPT, TSX, JAVASCRIPT, GO, JAVA, C, CPP, CSHARP] {
            assert!(compiled(spec).is_some(), "{} tags query must compile", spec.id);
        }
    }

    #[test]
    fn extensions_map() {
        assert_eq!(spec_for_path("src/main.rs").map(|s| s.id), Some("rust"));
        assert_eq!(spec_for_path("a/b.tsx").map(|s| s.id), Some("tsx"));
        assert!(spec_for_path("x.md").is_none());
    }
}

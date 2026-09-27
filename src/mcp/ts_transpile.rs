//! TypeScript → JavaScript transpilation via `swc` (pure Rust).
//!
//! The sandbox engine (QuickJS) only runs JavaScript, so TS snippets are
//! parsed as TypeScript, have their types stripped, and are emitted back as JS
//! source which is then handed to the same sandbox.
//!
//! This is a *syntactic* transform (like Node's type-stripping / bundlers):
//! annotations, interfaces, type aliases, generics, `as` / `satisfies` casts,
//! enums, and namespaces are handled; there is no type-checking (a type error
//! is not reported, matching how bundlers run TS).

use swc_core::common::{sync::Lrc, FileName, Globals, Mark, SourceMap, GLOBALS};
use swc_core::ecma::ast::{Pass, Program};
use swc_core::ecma::codegen::{text_writer::JsWriter, Config as CodegenConfig, Emitter};
use swc_core::ecma::parser::{lexer::Lexer, Parser, StringInput, Syntax, TsSyntax};
use swc_core::ecma::transforms::base::resolver;
use swc_core::ecma::transforms::typescript::strip;

/// Transpile TypeScript `source` to JavaScript. Returns the JS source, or an
/// error string describing the first parse failure.
pub fn transpile_ts(source: &str) -> Result<String, String> {
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(
        Lrc::new(FileName::Custom("repl.ts".into())),
        source.to_string(),
    );

    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax::default()),
        Default::default(),
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);

    let module = match parser.parse_module() {
        Ok(m) => m,
        Err(e) => return Err(format!("TypeScript syntax error: {e:?}")),
    };
    if let Some(e) = parser.take_errors().into_iter().next() {
        return Err(format!("TypeScript syntax error: {e:?}"));
    }

    GLOBALS.set(&Globals::new(), || {
        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();

        let mut program = Program::Module(module);
        resolver(unresolved_mark, top_level_mark, true).process(&mut program);
        strip(unresolved_mark, top_level_mark).process(&mut program);

        let mut buf = Vec::new();
        {
            let mut emitter = Emitter {
                cfg: CodegenConfig::default(),
                cm: cm.clone(),
                comments: None,
                wr: JsWriter::new(cm.clone(), "\n", &mut buf, None),
            };
            emitter
                .emit_program(&program)
                .map_err(|e| format!("codegen error: {e:?}"))?;
        }
        String::from_utf8(buf).map_err(|e| format!("utf8 error: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_type_annotations() {
        let js = transpile_ts("const x: number = 1 + 2; x").unwrap();
        assert!(js.contains("const x = 1 + 2"), "got: {js}");
        assert!(!js.contains(": number"), "types not stripped: {js}");
    }

    #[test]
    fn handles_interfaces_and_generics() {
        let src =
            "interface P { n: number }\nfunction id<T>(v: T): T { return v; }\nid<P>({ n: 5 }).n";
        let js = transpile_ts(src).unwrap();
        assert!(!js.contains("interface"), "interface not removed: {js}");
        assert!(js.contains("function id"), "got: {js}");
    }

    #[test]
    fn reports_syntax_error() {
        let err = transpile_ts("const x: = ;").unwrap_err();
        assert!(err.contains("syntax error"), "got: {err}");
    }
}

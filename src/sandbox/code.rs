//! Language support for "run this snippet".
//!
//! The snippet is never passed to the interpreter as an argument (`python -c
//! "…"`). It is written to a file in the sandbox directory and the interpreter
//! is pointed at the file. Two reasons: a snippet containing quotes stays
//! exactly the snippet the user wrote, and tracebacks carry a real filename and
//! line numbers instead of `<string>`.
//!
//! Because the sandbox directory is the same directory in both backends (docker
//! bind-mounts it at `/work`), the file is written once on the host and is
//! visible to whichever backend runs it.

use anyhow::{anyhow, Result};

#[derive(Debug)]
pub struct Lang {
    /// Canonical name recorded on the run.
    pub name: &'static str,
    /// Extension for the generated file.
    pub ext: &'static str,
    /// Interpreters to try, in order. The first one present wins.
    pub interpreters: &'static [&'static str],
}

const LANGS: &[Lang] = &[
    Lang { name: "python", ext: "py", interpreters: &["python3", "python"] },
    Lang { name: "javascript", ext: "js", interpreters: &["node", "deno", "bun"] },
    Lang { name: "typescript", ext: "ts", interpreters: &["deno", "bun", "ts-node"] },
    Lang { name: "bash", ext: "sh", interpreters: &["bash", "sh"] },
    Lang { name: "sh", ext: "sh", interpreters: &["sh"] },
    Lang { name: "ruby", ext: "rb", interpreters: &["ruby"] },
    Lang { name: "perl", ext: "pl", interpreters: &["perl"] },
    Lang { name: "php", ext: "php", interpreters: &["php"] },
];

pub fn languages() -> Vec<&'static str> {
    LANGS.iter().map(|l| l.name).collect()
}

/// Look up a language, accepting the usual aliases.
pub fn lookup(name: &str) -> Result<&'static Lang> {
    let n = name.trim().to_lowercase();
    let n = match n.as_str() {
        "py" | "python3" => "python",
        "js" | "node" => "javascript",
        "ts" => "typescript",
        "rb" => "ruby",
        "shell" | "zsh" => "bash",
        other => other,
    };
    LANGS
        .iter()
        .find(|l| l.name == n)
        .ok_or_else(|| anyhow!("language `{name}` is not supported; available: {}", languages().join(", ")))
}

/// The shell program that runs `file` with the right interpreter.
///
/// Interpreter choice happens **inside** the sandbox, not on the host: the
/// docker backend's interpreters are the image's, and the host has no way to
/// know what those are. When none is present the script says which ones it
/// looked for, because "exit 127" alone sends people hunting through their
/// image for the wrong problem.
pub fn launch_script(lang: &Lang, file: &str) -> String {
    let mut s = String::new();
    for interp in lang.interpreters {
        s.push_str(&format!(
            "if command -v {interp} >/dev/null 2>&1; then exec {interp} {file}; fi\n"
        ));
    }
    s.push_str(&format!(
        "echo 'No interpreter found for {} (tried: {}).' >&2\n\
         echo 'On the docker backend: pick an image that ships one, or install it in the sandbox and retry.' >&2\n\
         exit 127\n",
        lang.name,
        lang.interpreters.join(", ")
    ));
    s
}

/// Relative path of the generated file for a run.
pub fn source_path(run_id: &str, lang: &Lang) -> String {
    format!(".runs/{run_id}.{}", lang.ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve_to_the_canonical_language() {
        assert_eq!(lookup("py").unwrap().name, "python");
        assert_eq!(lookup("Python3").unwrap().name, "python");
        assert_eq!(lookup("JS").unwrap().name, "javascript");
        assert_eq!(lookup("zsh").unwrap().name, "bash");
    }

    #[test]
    fn an_unknown_language_lists_what_is_available() {
        let e = lookup("cobol").unwrap_err().to_string();
        assert!(e.contains("cobol"));
        assert!(e.contains("python"), "the error should name the supported set");
    }

    #[test]
    fn launch_script_tries_every_interpreter_in_order() {
        let s = launch_script(lookup("python").unwrap(), "a.py");
        let p3 = s.find("command -v python3").unwrap();
        let p = s.find("command -v python ").unwrap();
        assert!(p3 < p, "python3 must be preferred over python");
    }

    #[test]
    fn launch_script_fails_loudly_when_nothing_is_installed() {
        let s = launch_script(lookup("ruby").unwrap(), "a.rb");
        assert!(s.contains("exit 127"));
        assert!(s.contains("ruby"), "the error must name what was looked for");
    }

    #[test]
    fn the_snippet_is_referenced_by_file_never_inlined() {
        let s = launch_script(lookup("python").unwrap(), "x.py");
        assert!(s.contains("exec python3 x.py"));
        assert!(!s.contains("-c"), "`python -c` would re-quote the user's code");
    }

    #[test]
    fn generated_paths_live_in_a_dedicated_subdirectory() {
        let p = source_path("abc-123", lookup("python").unwrap());
        assert_eq!(p, ".runs/abc-123.py");
    }
}

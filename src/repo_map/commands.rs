//! How to build, test and lint the repository a chat is working in, read from
//! the files in it.
//!
//! A small model that has not been told `cargo test --lib foo` exists runs
//! `cargo test` over a 3,000-test workspace, or guesses `npm test` in a pnpm
//! repository and installs a second lockfile. The repository already says what
//! it uses: a `Cargo.toml`, a `package.json` with a `scripts` table and a
//! lockfile, a `Makefile`. This module reads those files — nothing runs — and
//! renders a few lines the first turn carries next to the project's own
//! instructions (`AGENTS.md` / `CLAUDE.md`).
//!
//! What it never does: execute anything, read outside the folder it is given,
//! or invent a command the files do not support. A script that is not in
//! `package.json` is not listed; a Makefile target is listed only if the
//! Makefile defines it.

use std::path::Path;

/// One ecosystem found in a folder, with the commands its files support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// `Rust`, `Node (pnpm)`, `Python`…
    pub name: String,
    /// The file that proved it (`Cargo.toml`).
    pub marker: &'static str,
    /// Folder relative to the one scanned (`""` for the root).
    pub dir: String,
    /// `(purpose, command)` — `("test", "cargo test")`.
    pub commands: Vec<(&'static str, String)>,
}

/// Longest `package.json` / `pyproject.toml` / `Makefile` we read.
const MAX_READ: u64 = 256 * 1024;

/// Subfolders looked at when the root has no manifest (monorepos).
const MAX_SUBDIRS: usize = 6;

fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_READ {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Every stack found at `dir`, or — when `dir` itself has none — in its
/// immediate subfolders (`app/`, `server/`, `web/`…).
pub fn detect(dir: &Path) -> Vec<Stack> {
    let mut found = stacks_in(dir, "");
    if !found.is_empty() {
        return found;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    let mut subs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.') && !matches!(n.as_str(), "node_modules" | "target" | "dist" | "build" | "vendor" | "venv" | "__pycache__"))
        .collect();
    subs.sort();
    for name in subs.into_iter().take(MAX_SUBDIRS * 3) {
        let sub = dir.join(&name);
        found.extend(stacks_in(&sub, &name));
        if found.len() >= MAX_SUBDIRS {
            break;
        }
    }
    found
}

fn stacks_in(dir: &Path, rel: &str) -> Vec<Stack> {
    let mut out = Vec::new();
    let prefix = |c: &str| if rel.is_empty() { c.to_string() } else { format!("cd {rel} && {c}") };
    let has = |f: &str| dir.join(f).is_file();

    if has("Cargo.toml") {
        let manifest = read_small(&dir.join("Cargo.toml")).unwrap_or_default();
        let workspace = manifest.contains("[workspace]");
        let mut commands = vec![
            ("build", prefix("cargo build")),
            ("test", prefix("cargo test")),
            ("test one", prefix("cargo test --lib <module::test_name>")),
            ("lint", prefix("cargo clippy --all-targets")),
            ("format", prefix("cargo fmt")),
        ];
        if workspace {
            commands.insert(2, ("test crate", prefix("cargo test -p <crate>")));
        }
        out.push(Stack { name: "Rust".into(), marker: "Cargo.toml", dir: rel.into(), commands });
    }

    if has("package.json") {
        out.push(node_stack(dir, rel));
    }

    let py_marker = ["pyproject.toml", "pytest.ini", "tox.ini", "setup.cfg", "requirements.txt", "setup.py"].into_iter().find(|f| has(f));
    if let Some(marker) = py_marker {
        let toml = read_small(&dir.join("pyproject.toml")).unwrap_or_default();
        let uses = |t: &str| toml.contains(t) || has(&format!("{t}.toml")) || has(&format!(".{t}.toml")) || has(&format!(".{t}.ini"));
        let runner = if has("uv.lock") {
            "uv run "
        } else if has("poetry.lock") {
            "poetry run "
        } else {
            ""
        };
        let mut commands = vec![("test", prefix(&format!("{runner}pytest"))), ("test one", prefix(&format!("{runner}pytest path/to/test_file.py::test_name")))];
        if uses("ruff") {
            commands.push(("lint", prefix(&format!("{runner}ruff check ."))));
            commands.push(("format", prefix(&format!("{runner}ruff format ."))));
        }
        if uses("mypy") {
            commands.push(("typecheck", prefix(&format!("{runner}mypy ."))));
        }
        out.push(Stack { name: "Python".into(), marker, dir: rel.into(), commands });
    }

    if has("go.mod") {
        out.push(Stack {
            name: "Go".into(),
            marker: "go.mod",
            dir: rel.into(),
            commands: vec![
                ("build", prefix("go build ./...")),
                ("test", prefix("go test ./...")),
                ("test one", prefix("go test ./path/to/pkg -run TestName")),
                ("lint", prefix("go vet ./...")),
                ("format", prefix("gofmt -l .")),
            ],
        });
    }

    if has("pubspec.yaml") {
        let spec = read_small(&dir.join("pubspec.yaml")).unwrap_or_default();
        let flutter = spec.lines().any(|l| l.trim_start().starts_with("flutter:") || l.contains("sdk: flutter"));
        let tool = if flutter { "flutter" } else { "dart" };
        out.push(Stack {
            name: if flutter { "Flutter".into() } else { "Dart".into() },
            marker: "pubspec.yaml",
            dir: rel.into(),
            commands: vec![
                ("test", prefix(&format!("{tool} test"))),
                ("lint", prefix(&format!("{tool} analyze"))),
                ("format", prefix("dart format .")),
            ],
        });
    }

    if has("Package.swift") {
        out.push(Stack {
            name: "Swift".into(),
            marker: "Package.swift",
            dir: rel.into(),
            commands: vec![("build", prefix("swift build")), ("test", prefix("swift test"))],
        });
    }

    if has("pom.xml") {
        let wrapper = if has("mvnw") { "./mvnw" } else { "mvn" };
        out.push(Stack {
            name: "Maven".into(),
            marker: "pom.xml",
            dir: rel.into(),
            commands: vec![("build", prefix(&format!("{wrapper} -q package -DskipTests"))), ("test", prefix(&format!("{wrapper} test")))],
        });
    }

    let gradle = ["build.gradle", "build.gradle.kts"].into_iter().find(|f| has(f));
    if let Some(marker) = gradle {
        let wrapper = if has("gradlew") { "./gradlew" } else { "gradle" };
        out.push(Stack {
            name: "Gradle".into(),
            marker,
            dir: rel.into(),
            commands: vec![("build", prefix(&format!("{wrapper} build -x test"))), ("test", prefix(&format!("{wrapper} test")))],
        });
    }

    // A Makefile adds the targets it defines, and nothing else.
    for name in ["Makefile", "makefile", "GNUmakefile"] {
        if let Some(text) = read_small(&dir.join(name)) {
            let targets = make_targets(&text);
            let mut commands = Vec::new();
            for (purpose, candidates) in [("build", &["build", "all"][..]), ("test", &["test", "check"][..]), ("lint", &["lint", "clippy"][..]), ("format", &["fmt", "format"][..])] {
                if let Some(t) = candidates.iter().find(|c| targets.iter().any(|x| x == *c)) {
                    commands.push((purpose, prefix(&format!("make {t}"))));
                }
            }
            if !commands.is_empty() {
                out.push(Stack { name: "make".into(), marker: "Makefile", dir: rel.into(), commands });
            }
            break;
        }
    }

    out
}

fn node_stack(dir: &Path, rel: &str) -> Stack {
    let pm = if dir.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if dir.join("yarn.lock").is_file() {
        "yarn"
    } else if dir.join("bun.lockb").is_file() || dir.join("bun.lock").is_file() {
        "bun"
    } else {
        "npm"
    };
    let prefix = |c: &str| if rel.is_empty() { c.to_string() } else { format!("cd {rel} && {c}") };
    let scripts: serde_json::Map<String, serde_json::Value> = read_small(&dir.join("package.json"))
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("scripts").and_then(|s| s.as_object().cloned()))
        .unwrap_or_default();
    let run = |script: &str| match (pm, script) {
        (_, "test") => format!("{pm} test"),
        ("npm", s) => format!("npm run {s}"),
        (pm, s) => format!("{pm} run {s}"),
    };
    let mut commands = Vec::new();
    for (purpose, candidates) in [
        ("test", &["test", "test:unit"][..]),
        ("build", &["build"][..]),
        ("lint", &["lint", "eslint"][..]),
        ("typecheck", &["typecheck", "type-check", "tsc", "check"][..]),
        ("format", &["format", "prettier"][..]),
        ("dev", &["dev", "start"][..]),
    ] {
        if let Some(s) = candidates.iter().find(|c| scripts.contains_key(**c)) {
            commands.push((purpose, prefix(&run(s))));
        }
    }
    if commands.is_empty() {
        commands.push(("install", prefix(&format!("{pm} install"))));
    }
    let ts = dir.join("tsconfig.json").is_file();
    Stack { name: format!("Node{} ({pm})", if ts { "/TypeScript" } else { "" }), marker: "package.json", dir: rel.into(), commands }
}

/// Targets a Makefile defines: `name:` at the start of a line, not a variable
/// assignment (`CC := gcc`) and not a pattern rule (`%.o: %.c`).
fn make_targets(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.starts_with(['\t', ' ', '#', '.']))
        .filter_map(|l| {
            let (head, rest) = l.split_once(':')?;
            if rest.starts_with('=') || head.contains(['=', '%', '$']) {
                return None;
            }
            Some(head.split_whitespace().map(str::to_string).collect::<Vec<_>>())
        })
        .flatten()
        .collect()
}

/// The block the first turn carries, or `None` when the folder says nothing
/// about how to build it.
pub fn render(dir: &Path) -> Option<String> {
    let stacks = detect(dir);
    if stacks.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<project_commands>\nRead from the files in this folder — prefer these over guessing; run the narrowest one that checks your change first.\n",
    );
    for s in stacks.iter().take(8) {
        let place = if s.dir.is_empty() { String::new() } else { format!(" in {}/", s.dir) };
        out.push_str(&format!("- {} ({}){}: ", s.name, s.marker, place));
        let cmds: Vec<String> = s.commands.iter().map(|(p, c)| format!("{p} `{c}`")).collect();
        out.push_str(&cmds.join(" · "));
        out.push('\n');
    }
    out.push_str("</project_commands>");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = d.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        d
    }

    fn cmd<'a>(s: &'a Stack, purpose: &str) -> Option<&'a str> {
        s.commands.iter().find(|(p, _)| *p == purpose).map(|(_, c)| c.as_str())
    }

    #[test]
    fn a_rust_workspace_gets_cargo_and_a_per_crate_test() {
        let d = dir_with(&[("Cargo.toml", "[workspace]\nmembers = [\"a\"]\n")]);
        let s = detect(d.path());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "Rust");
        assert_eq!(cmd(&s[0], "test"), Some("cargo test"));
        assert_eq!(cmd(&s[0], "test crate"), Some("cargo test -p <crate>"));
    }

    #[test]
    fn node_uses_the_lockfiles_package_manager_and_only_scripts_that_exist() {
        let d = dir_with(&[
            ("package.json", r#"{"scripts":{"test":"vitest run","build":"vite build","lint":"eslint ."}}"#),
            ("pnpm-lock.yaml", ""),
            ("tsconfig.json", "{}"),
        ]);
        let s = detect(d.path());
        assert_eq!(s[0].name, "Node/TypeScript (pnpm)");
        assert_eq!(cmd(&s[0], "test"), Some("pnpm test"));
        assert_eq!(cmd(&s[0], "build"), Some("pnpm run build"));
        assert_eq!(cmd(&s[0], "lint"), Some("pnpm run lint"));
        assert_eq!(cmd(&s[0], "typecheck"), None, "no typecheck script, none invented");
    }

    #[test]
    fn npm_is_the_default_and_a_scriptless_package_says_install() {
        let d = dir_with(&[("package.json", r#"{"name":"x"}"#)]);
        let s = detect(d.path());
        assert_eq!(cmd(&s[0], "install"), Some("npm install"));
        let d = dir_with(&[("package.json", r#"{"scripts":{"build":"tsc"}}"#)]);
        assert_eq!(cmd(&detect(d.path())[0], "build"), Some("npm run build"));
    }

    #[test]
    fn python_follows_the_runner_and_tools_it_configures() {
        let d = dir_with(&[("pyproject.toml", "[tool.ruff]\nline-length = 100\n[tool.mypy]\n"), ("uv.lock", "")]);
        let s = detect(d.path());
        assert_eq!(cmd(&s[0], "test"), Some("uv run pytest"));
        assert_eq!(cmd(&s[0], "lint"), Some("uv run ruff check ."));
        assert_eq!(cmd(&s[0], "typecheck"), Some("uv run mypy ."));
        let d = dir_with(&[("requirements.txt", "flask\n")]);
        assert_eq!(cmd(&detect(d.path())[0], "lint"), None);
    }

    #[test]
    fn a_makefile_lists_only_targets_it_defines() {
        let d = dir_with(&[("Makefile", "CC := gcc\n.PHONY: test\ntest:\n\tgo test\n%.o: %.c\n\t$(CC)\nlint:\n\techo\n")]);
        let s = detect(d.path());
        let make = s.iter().find(|s| s.name == "make").unwrap();
        assert_eq!(cmd(make, "test"), Some("make test"));
        assert_eq!(cmd(make, "lint"), Some("make lint"));
        assert_eq!(cmd(make, "build"), None);
    }

    #[test]
    fn flutter_is_told_from_dart_by_the_sdk_dependency() {
        let d = dir_with(&[("pubspec.yaml", "name: a\ndependencies:\n  flutter:\n    sdk: flutter\n")]);
        let s = detect(d.path());
        assert_eq!(s[0].name, "Flutter");
        assert_eq!(cmd(&s[0], "lint"), Some("flutter analyze"));
        let d = dir_with(&[("pubspec.yaml", "name: a\n")]);
        assert_eq!(detect(d.path())[0].name, "Dart");
    }

    #[test]
    fn a_monorepo_without_a_root_manifest_is_read_one_level_down() {
        let d = dir_with(&[("server/go.mod", "module x\n"), ("web/package.json", r#"{"scripts":{"test":"jest"}}"#), ("node_modules/x/package.json", "{}")]);
        let s = detect(d.path());
        let names: Vec<_> = s.iter().map(|s| (s.name.as_str(), s.dir.as_str())).collect();
        assert!(names.contains(&("Go", "server")), "{names:?}");
        assert!(names.iter().any(|(n, d)| n.starts_with("Node") && *d == "web"), "{names:?}");
        assert!(!names.iter().any(|(_, d)| *d == "node_modules"));
        assert_eq!(cmd(s.iter().find(|s| s.dir == "server").unwrap(), "test"), Some("cd server && go test ./..."));
    }

    #[test]
    fn a_folder_that_says_nothing_renders_nothing() {
        assert_eq!(render(dir_with(&[("notes.txt", "hi")]).path()), None);
        assert_eq!(render(Path::new("/definitely/not/a/dir")), None);
    }

    #[test]
    fn the_block_is_short_and_names_each_stack_once() {
        let d = dir_with(&[("Cargo.toml", "[package]\n"), ("package.json", r#"{"scripts":{"test":"x"}}"#)]);
        let text = render(d.path()).unwrap();
        assert!(text.starts_with("<project_commands>") && text.ends_with("</project_commands>"));
        assert_eq!(text.matches("- Rust").count(), 1);
        assert!(text.len() < 1400, "{} chars", text.len());
    }

    #[test]
    fn an_oversized_manifest_is_not_read() {
        let d = dir_with(&[("package.json", &format!("{{\"scripts\":{{\"test\":\"{}\"}}}}", "x".repeat(300_000)))]);
        let s = detect(d.path());
        assert_eq!(cmd(&s[0], "install"), Some("npm install"), "unreadable scripts fall back, never panic");
    }
}

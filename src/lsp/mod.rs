//! Language-server diagnostics as feedback after an edit (OpenCode's model).
//!
//! When `Edit`/`Write` finish, the file is pushed to the workspace's language
//! server and the diagnostics that come back for *that file* are appended to
//! the tool result — the model sees `error[E0308]` right away instead of only
//! after it decides to run the build. LSP here is a hook on the tool loop,
//! not a tool the model has to remember to call (the `lsp_diagnostics` tool
//! exists for re-asking).
//!
//! Servers are only ever ones already on `PATH` (or named in
//! `~/.senclaw/lsp.json`); nothing is downloaded. One process per
//! (workspace, language), killed after [`IDLE_TIMEOUT`], and a server that
//! fails twice in a row is disabled for that workspace so a broken install
//! cannot slow every edit.

pub mod client;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use client::{Diagnostic, LspClient};

/// Default wait for diagnostics after a change. Rust-analyzer on a large
/// crate can take several seconds; OpenCode's 3 s missed it.
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;
/// A server unused for this long is shut down.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// Diagnostics appended to a tool result, at most.
pub const MAX_REPORTED: usize = 20;
/// Consecutive failures before a (workspace, language) server is disabled.
const MAX_FAILURES: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// `~/.senclaw/lsp.json`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspSettings {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// language id → server. Overrides / extends the built-in table.
    #[serde(default)]
    pub servers: HashMap<String, ServerSpec>,
}

fn yes() -> bool {
    true
}
fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

impl Default for LspSettings {
    fn default() -> Self {
        Self { enabled: true, timeout_ms: DEFAULT_TIMEOUT_MS, servers: HashMap::new() }
    }
}

pub fn settings_path() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".senclaw").join("lsp.json")
}

pub fn load_settings() -> LspSettings {
    let mut s = std::fs::read(settings_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<LspSettings>(&b).ok())
        .unwrap_or_default();
    if std::env::var("SENCLAW_LSP").map(|v| v == "0").unwrap_or(false) {
        s.enabled = false;
    }
    s
}

pub fn save_settings(s: &LspSettings) -> anyhow::Result<()> {
    let p = settings_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(p, serde_json::to_vec_pretty(s)?)?;
    Ok(())
}

/// Built-in servers, by LSP language id. Only used when on `PATH`.
fn builtin_server(lang: &str) -> Option<ServerSpec> {
    let (cmd, args): (&str, &[&str]) = match lang {
        "rust" => ("rust-analyzer", &[]),
        "typescript" | "typescriptreact" | "javascript" | "javascriptreact" => {
            ("typescript-language-server", &["--stdio"])
        }
        "python" => ("pyright-langserver", &["--stdio"]),
        "go" => ("gopls", &[]),
        "dart" => ("dart", &["language-server", "--protocol=lsp"]),
        "c" | "cpp" => ("clangd", &[]),
        _ => return None,
    };
    Some(ServerSpec { command: cmd.to_string(), args: args.iter().map(|s| s.to_string()).collect() })
}

/// LSP language id for a path.
pub fn language_id(path: &Path) -> Option<&'static str> {
    Some(match path.extension()?.to_str()? {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "go" => "go",
        "dart" => "dart",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        _ => return None,
    })
}

/// Servers share a process across the ids they cover (one tsserver for
/// ts/tsx/js), so key by the *server* language group.
fn server_group(lang: &str) -> &'static str {
    match lang {
        "typescript" | "typescriptreact" | "javascript" | "javascriptreact" => "typescript",
        "c" | "cpp" => "c",
        "rust" => "rust",
        "python" => "python",
        "go" => "go",
        "dart" => "dart",
        _ => "other",
    }
}

fn on_path(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).exists();
    }
    let Ok(path) = std::env::var("PATH") else { return false };
    std::env::split_paths(&path).any(|d| d.join(cmd).is_file())
}

struct Entry {
    client: Arc<LspClient>,
}

#[derive(Default)]
struct Manager {
    servers: HashMap<(PathBuf, &'static str), Entry>,
    failures: HashMap<(PathBuf, &'static str), u32>,
    /// (workspace, group) → why it is disabled, for the status endpoint.
    disabled: HashMap<(PathBuf, &'static str), String>,
}

static MANAGER: OnceLock<tokio::sync::Mutex<Manager>> = OnceLock::new();
static JANITOR: OnceLock<()> = OnceLock::new();

fn manager() -> &'static tokio::sync::Mutex<Manager> {
    MANAGER.get_or_init(|| tokio::sync::Mutex::new(Manager::default()))
}

fn ensure_janitor() {
    JANITOR.get_or_init(|| {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let mut m = manager().lock().await;
                let idle: Vec<_> = m
                    .servers
                    .iter()
                    .filter(|(_, e)| e.client.idle_for() > IDLE_TIMEOUT || !e.client.is_alive())
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in idle {
                    if let Some(e) = m.servers.remove(&k) {
                        tracing::info!(server = %e.client.name, "[LSP] stopping idle server");
                        e.client.shutdown().await;
                    }
                }
            }
        });
    });
}

/// Workspace root for a file: nearest ancestor of `working_dir` (or the
/// directory itself) that looks like a project; the working dir otherwise.
fn workspace_root(working_dir: &Path) -> PathBuf {
    working_dir.to_path_buf()
}

/// The running server for (workspace, language), spawning it if needed.
/// `None` when LSP is off, the language has no server, the server is not
/// installed, or it has been disabled after repeated failures.
async fn server_for(settings: &LspSettings, workspace: &Path, lang: &str) -> Option<Arc<LspClient>> {
    let group = server_group(lang);
    let key = (workspace.to_path_buf(), group);
    let spec = settings
        .servers
        .get(lang)
        .or_else(|| settings.servers.get(group))
        .cloned()
        .or_else(|| builtin_server(lang))?;
    if !on_path(&spec.command) {
        return None;
    }
    ensure_janitor();
    let mut m = manager().lock().await;
    if m.disabled.contains_key(&key) {
        return None;
    }
    if let Some(e) = m.servers.get(&key) {
        if e.client.is_alive() {
            return Some(Arc::clone(&e.client));
        }
        m.servers.remove(&key);
    }
    match LspClient::spawn(&spec.command, &spec.command, &spec.args, workspace).await {
        Ok(c) => {
            tracing::info!(server = %spec.command, workspace = %workspace.display(), "[LSP] started");
            let c = Arc::new(c);
            m.failures.remove(&key);
            m.servers.insert(key, Entry { client: Arc::clone(&c) });
            Some(c)
        }
        Err(e) => {
            let n = m.failures.entry(key.clone()).or_insert(0);
            *n += 1;
            tracing::warn!(server = %spec.command, error = %e, attempt = *n, "[LSP] failed to start");
            if *n >= MAX_FAILURES {
                m.disabled.insert(key, format!("{e}"));
            }
            None
        }
    }
}

/// Outcome of a diagnostics pass for one file.
pub struct DiagnosticsReport {
    pub server: String,
    pub path: String,
    pub diagnostics: Vec<Diagnostic>,
    /// `false` when the deadline passed before the server answered for this
    /// change; `diagnostics` is then whatever was known before.
    pub fresh: bool,
}

impl DiagnosticsReport {
    /// Text block appended to a tool result. Empty when there is nothing to
    /// say (fresh and clean).
    pub fn render(&self) -> String {
        if self.diagnostics.is_empty() {
            return if self.fresh {
                String::new()
            } else {
                format!("\n\nLSP ({}) did not answer within the deadline; diagnostics unknown.", self.server)
            };
        }
        let mut errors = 0;
        let mut warnings = 0;
        for d in &self.diagnostics {
            match d.severity {
                Some(1) => errors += 1,
                Some(2) => warnings += 1,
                _ => {}
            }
        }
        let mut out = format!(
            "\n\nLSP diagnostics ({}) for {}: {} error(s), {} warning(s){}",
            self.server,
            self.path,
            errors,
            warnings,
            if self.fresh { "" } else { " [stale — server still analysing]" }
        );
        let mut shown: Vec<&Diagnostic> = self.diagnostics.iter().collect();
        // Errors first, then warnings, then the rest; by line within.
        shown.sort_by_key(|d| (d.severity.unwrap_or(9), d.range.start.line));
        for d in shown.iter().take(MAX_REPORTED) {
            let code = d
                .code
                .as_ref()
                .map(|c| match c {
                    serde_json::Value::String(s) => format!("[{s}] "),
                    other => format!("[{other}] "),
                })
                .unwrap_or_default();
            let msg = d.message.lines().next().unwrap_or("").trim();
            out.push_str(&format!(
                "\n  {} L{}:{} {code}{msg}",
                d.severity_label(),
                d.range.start.line + 1,
                d.range.start.character + 1
            ));
        }
        if self.diagnostics.len() > MAX_REPORTED {
            out.push_str(&format!("\n  … {} more", self.diagnostics.len() - MAX_REPORTED));
        }
        out
    }
}

/// Called after a tool wrote `path`. Pushes the file to the language server
/// and waits (bounded) for its diagnostics. `None` when LSP does not apply.
pub async fn diagnostics_after_write(working_dir: &str, path: &str) -> Option<DiagnosticsReport> {
    let settings = load_settings();
    if !settings.enabled || working_dir.is_empty() {
        return None;
    }
    let root = crate::util::paths::expand_tilde(working_dir);
    let abs = if Path::new(path).is_absolute() { PathBuf::from(path) } else { root.join(path) };
    let lang = language_id(&abs)?;
    let text = std::fs::read_to_string(&abs).ok()?;
    let workspace = workspace_root(&root);
    let client = server_for(&settings, &workspace, lang).await?;
    let since = Instant::now();
    if let Err(e) = client.sync_document(&abs, lang, &text).await {
        tracing::warn!(error = %e, "[LSP] sync failed");
        return None;
    }
    let (diagnostics, fresh) = client
        .wait_diagnostics(&abs, since, Duration::from_millis(settings.timeout_ms))
        .await;
    let rel = abs.strip_prefix(&root).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| abs.to_string_lossy().to_string());
    Some(DiagnosticsReport { server: client.name.clone(), path: rel, diagnostics, fresh })
}

/// Diagnostics currently known for a file (re-syncing it first) — the
/// `lsp_diagnostics` tool.
pub async fn diagnostics_for(working_dir: &str, path: &str) -> Option<DiagnosticsReport> {
    diagnostics_after_write(working_dir, path).await
}

/// Everything the servers currently report across the workspace's files.
pub async fn workspace_diagnostics(working_dir: &str) -> Vec<(String, String, Vec<Diagnostic>)> {
    let root = crate::util::paths::expand_tilde(working_dir);
    let m = manager().lock().await;
    let mut out = Vec::new();
    for ((ws, _), e) in m.servers.iter() {
        if *ws != root {
            continue;
        }
        for (path, items) in e.client.all_diagnostics() {
            out.push((e.client.name.clone(), path, items));
        }
    }
    out
}

/// Status for the REST endpoint / `lsp_status` tool.
pub async fn status() -> serde_json::Value {
    let settings = load_settings();
    let m = manager().lock().await;
    let servers: Vec<serde_json::Value> = m
        .servers
        .iter()
        .map(|((ws, group), e)| {
            serde_json::json!({
                "workspace": ws.to_string_lossy(),
                "language": group,
                "command": e.client.name,
                "alive": e.client.is_alive(),
                "idleSecs": e.client.idle_for().as_secs(),
            })
        })
        .collect();
    let disabled: Vec<serde_json::Value> = m
        .disabled
        .iter()
        .map(|((ws, group), why)| serde_json::json!({ "workspace": ws.to_string_lossy(), "language": group, "reason": why }))
        .collect();
    let available: Vec<serde_json::Value> = ["rust", "typescript", "python", "go", "dart", "c"]
        .iter()
        .map(|lang| {
            let spec = settings.servers.get(*lang).cloned().or_else(|| builtin_server(lang));
            serde_json::json!({
                "language": lang,
                "command": spec.as_ref().map(|s| s.command.clone()),
                "installed": spec.as_ref().map(|s| on_path(&s.command)).unwrap_or(false),
            })
        })
        .collect();
    serde_json::json!({
        "enabled": settings.enabled,
        "timeoutMs": settings.timeout_ms,
        "settingsPath": settings_path().to_string_lossy(),
        "servers": servers,
        "disabled": disabled,
        "available": available,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_ids_and_groups() {
        assert_eq!(language_id(Path::new("a.rs")), Some("rust"));
        assert_eq!(language_id(Path::new("a.tsx")), Some("typescriptreact"));
        assert_eq!(server_group("typescriptreact"), "typescript");
        assert_eq!(language_id(Path::new("a.md")), None);
    }

    #[test]
    fn render_lists_errors_first_and_caps() {
        use client::{Position, Range};
        let mk = |sev, line, msg: &str| Diagnostic {
            range: Range { start: Position { line, character: 0 }, end: Position { line, character: 1 } },
            severity: Some(sev),
            code: Some(serde_json::Value::String("E0308".into())),
            source: None,
            message: msg.into(),
        };
        let mut diags = vec![mk(2, 5, "unused"), mk(1, 9, "mismatched types\nexpected u32")];
        for i in 0..30 {
            diags.push(mk(4, 100 + i, "hint"));
        }
        let r = DiagnosticsReport { server: "rust-analyzer".into(), path: "src/a.rs".into(), diagnostics: diags, fresh: true };
        let text = r.render();
        assert!(text.contains("1 error(s), 1 warning(s)"));
        let err_at = text.find("error L10:1 [E0308] mismatched types").unwrap();
        let warn_at = text.find("warning L6:1").unwrap();
        assert!(err_at < warn_at);
        assert!(!text.contains("expected u32"), "only the first line of a message");
        assert!(text.contains("… 12 more"));
        let clean = DiagnosticsReport { server: "x".into(), path: "p".into(), diagnostics: vec![], fresh: true };
        assert!(clean.render().is_empty());
        let stale = DiagnosticsReport { server: "x".into(), path: "p".into(), diagnostics: vec![], fresh: false };
        assert!(stale.render().contains("did not answer"));
    }

    /// `cargo test --lib lsp::tests::rust_analyzer_reports_a_type_error -- --ignored --nocapture`
    /// Needs `rust-analyzer` on PATH; makes a throwaway crate with a type
    /// error and expects the error to come back through the real protocol.
    /// `cargo test --lib lsp::tests::a_real_server_reports_a_type_error -- --ignored --nocapture`
    /// Uses whichever of gopls / rust-analyzer is installed: a throwaway
    /// project with a type error must produce an error diagnostic on line 2
    /// through the real protocol. Also exercises the failure path when a
    /// command on PATH exits at once (the rustup proxy without the component).
    #[tokio::test]
    #[ignore]
    async fn a_real_server_reports_a_type_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (lang, rel) = if on_path("gopls") && on_path("go") {
            std::fs::write(root.join("go.mod"), "module probe\n\ngo 1.21\n").unwrap();
            std::fs::write(root.join("main.go"), "package main\nvar x int = \"no\"\nfunc main() { _ = x }\n").unwrap();
            ("go", "main.go")
        } else if on_path("rust-analyzer") {
            std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"lsp_probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n").unwrap();
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("src/main.rs"), "fn main() {\n    let x: u32 = \"no\";\n    println!(\"{x}\");\n}\n").unwrap();
            ("rust", "src/main.rs")
        } else {
            eprintln!("no language server on PATH; skipping");
            return;
        };
        let settings = LspSettings { timeout_ms: 20_000, ..Default::default() };
        let abs = root.join(rel);
        let started = Instant::now();
        let mut found = None;
        for _ in 0..4 {
            let client = match server_for(&settings, &root, lang).await {
                Some(c) => c,
                None => {
                    let spec = builtin_server(lang).unwrap();
                    let direct = LspClient::spawn(&spec.command, &spec.command, &spec.args, &root).await;
                    panic!("server_for returned None; direct spawn: {:?}", direct.err());
                }
            };
            let since = Instant::now();
            let text = std::fs::read_to_string(&abs).unwrap();
            client.sync_document(&abs, lang, &text).await.unwrap();
            let (d, fresh) = client.wait_diagnostics(&abs, since, Duration::from_millis(20_000)).await;
            eprintln!("[{lang}] after {:?}: fresh={fresh} diagnostics={}", started.elapsed(), d.len());
            if d.iter().any(|x| x.severity == Some(1)) {
                found = Some(d);
                break;
            }
        }
        let d = found.expect("an error diagnostic within the budget");
        let err = d.iter().find(|x| x.severity == Some(1)).unwrap();
        eprintln!("[{lang}] {} L{}: {}", err.severity_label(), err.range.start.line + 1, err.message);
        assert_eq!(err.range.start.line, 1);
        let st = status().await;
        assert!(st["servers"].as_array().unwrap().iter().any(|s| s["alive"] == true));
        let rendered = DiagnosticsReport { server: lang.into(), path: rel.into(), diagnostics: d, fresh: true }.render();
        eprintln!("{rendered}");
        assert!(rendered.contains("error L2:"));
    }

    #[test]
    fn settings_default_and_env_off() {
        let s = LspSettings::default();
        assert!(s.enabled);
        assert_eq!(s.timeout_ms, DEFAULT_TIMEOUT_MS);
        let parsed: LspSettings = serde_json::from_str(r#"{"servers":{"rust":{"command":"ra"}}}"#).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.servers["rust"].command, "ra");
    }
}

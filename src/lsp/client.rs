//! A minimal Language Server Protocol client over stdio.
//!
//! Enough of the protocol for one purpose — *diagnostics after an edit*:
//! `initialize`, `textDocument/didOpen` / `didChange` (full text) /
//! `didSave`, and receiving `textDocument/publishDiagnostics`. No completion,
//! no hover, no workspace edits. Types are hand-written for the fields used
//! rather than pulling in the full `lsp-types` crate.
//!
//! OpenCode's three documented traps are handled here by construction:
//! - `didChange` is always sent for a file already open, so a server that
//!   keeps the document in memory (most do) sees the new text instead of
//!   re-reporting stale diagnostics (opencode#12288).
//! - Waiting is per URI: diagnostics that arrive for *other* files — the
//!   rest of the crate, another project — are stored but never returned as
//!   this file's (opencode#16353).
//! - The wait has a caller-chosen deadline and returns what has arrived by
//!   then; the tool loop is never held by a slow server (opencode#16880).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Notify};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    /// 1 error, 2 warning, 3 information, 4 hint (LSP `DiagnosticSeverity`).
    #[serde(default)]
    pub severity: Option<u8>,
    #[serde(default)]
    pub code: Option<serde_json::Value>,
    #[serde(default)]
    pub source: Option<String>,
    pub message: String,
}

impl Diagnostic {
    pub fn severity_label(&self) -> &'static str {
        match self.severity {
            Some(1) => "error",
            Some(2) => "warning",
            Some(3) => "info",
            Some(4) => "hint",
            _ => "diagnostic",
        }
    }
}

#[derive(Debug, Deserialize)]
struct PublishDiagnosticsParams {
    uri: String,
    #[serde(default)]
    version: Option<i64>,
    #[serde(default)]
    diagnostics: Vec<Diagnostic>,
}

/// Diagnostics for one document plus when they arrived, so a waiter can tell
/// "new since my edit" from "left over from before".
#[derive(Debug, Clone)]
pub struct DocDiagnostics {
    pub version: Option<i64>,
    pub received_at: Instant,
    pub items: Vec<Diagnostic>,
}

struct Inner {
    stdin: tokio::sync::Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<serde_json::Value>>>,
    diagnostics: Mutex<HashMap<String, DocDiagnostics>>,
    diag_notify: Notify,
    open_versions: Mutex<HashMap<String, i64>>,
    last_used: AtomicU64,
    started: Instant,
}

pub struct LspClient {
    pub name: String,
    inner: Arc<Inner>,
    child: Mutex<Option<Child>>,
}

pub fn file_uri(path: &Path) -> String {
    let p = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !p.starts_with('/') {
        out.push('/');
    }
    for ch in p.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '/' | '-' | '.' | '_' | '~' => out.push(ch),
            ' ' => out.push_str("%20"),
            other => {
                let mut buf = [0u8; 4];
                for b in other.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

impl LspClient {
    /// Spawn `command args` in `root` and complete the initialize handshake.
    pub async fn spawn(name: &str, command: &str, args: &[String], root: &Path) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning {command}"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        // Keep the tail of stderr so a server that exits at once (the rustup
        // proxy without the component installed, a missing node module) is
        // reported with its own words instead of "connection closed".
        let stderr_tail: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&stderr_tail);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let mut t = tail.lock().unwrap();
                    if t.len() > 2000 {
                        t.clear();
                    }
                    t.push_str(&l);
                    t.push('\n');
                }
            });
        }
        let inner = Arc::new(Inner {
            stdin: tokio::sync::Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            diag_notify: Notify::new(),
            open_versions: Mutex::new(HashMap::new()),
            last_used: AtomicU64::new(0),
            started: Instant::now(),
        });
        let reader_inner = Arc::clone(&inner);
        tokio::spawn(async move { read_loop(stdout, reader_inner).await });

        let client = Self { name: name.to_string(), inner, child: Mutex::new(Some(child)) };
        let root_uri = file_uri(root);
        let init = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default() }],
            "capabilities": {
                "textDocument": {
                    "synchronization": { "didSave": true, "dynamicRegistration": false },
                    "publishDiagnostics": { "relatedInformation": false, "versionSupport": true }
                },
                "workspace": { "workspaceFolders": true }
            },
            "clientInfo": { "name": "senclaw", "version": env!("CARGO_PKG_VERSION") }
        });
        let init_result = tokio::time::timeout(Duration::from_secs(30), client.request("initialize", init))
            .await
            .map_err(|_| anyhow!("{name}: initialize timed out"));
        if let Err(e) = init_result.and_then(|r| r) {
            let tail = stderr_tail.lock().unwrap().trim().to_string();
            return Err(if tail.is_empty() {
                e
            } else {
                anyhow!("{e} (stderr: {})", tail.lines().last().unwrap_or(""))
            });
        }
        client.notify("initialized", serde_json::json!({})).await?;
        Ok(client)
    }

    pub fn touch(&self) {
        self.inner
            .last_used
            .store(self.inner.started.elapsed().as_secs(), Ordering::Relaxed);
    }

    pub fn idle_for(&self) -> Duration {
        let last = self.inner.last_used.load(Ordering::Relaxed);
        self.inner.started.elapsed().saturating_sub(Duration::from_secs(last))
    }

    pub fn is_alive(&self) -> bool {
        match self.child.lock().unwrap().as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    async fn send(&self, msg: &serde_json::Value) -> Result<()> {
        let body = serde_json::to_vec(msg)?;
        let mut stdin = self.inner.stdin.lock().await;
        stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await?;
        stdin.write_all(&body).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn request(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        self.send(&serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        let resp = rx.await.map_err(|_| anyhow!("{}: connection closed", self.name))?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("{}: {method} failed: {err}", self.name));
        }
        Ok(resp.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }

    pub async fn notify(&self, method: &str, params: serde_json::Value) -> Result<()> {
        self.send(&serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// Open the document if new, otherwise send its full new text as a
    /// change, then a save notification. Returns the version sent.
    pub async fn sync_document(&self, path: &Path, language_id: &str, text: &str) -> Result<i64> {
        self.touch();
        let uri = file_uri(path);
        let version = {
            let mut open = self.inner.open_versions.lock().unwrap();
            let v = open.entry(uri.clone()).and_modify(|v| *v += 1).or_insert(0);
            *v
        };
        if version == 0 {
            self.notify(
                "textDocument/didOpen",
                serde_json::json!({ "textDocument": { "uri": uri, "languageId": language_id, "version": 0, "text": text } }),
            )
            .await?;
        } else {
            self.notify(
                "textDocument/didChange",
                serde_json::json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text }] }),
            )
            .await?;
        }
        self.notify("textDocument/didSave", serde_json::json!({ "textDocument": { "uri": uri } }))
            .await?;
        Ok(version)
    }

    /// Wait until diagnostics for `path` newer than `since` arrive, or
    /// `timeout` passes. Returns whatever is stored for the file at that point
    /// (possibly older diagnostics, flagged by `fresh = false`).
    pub async fn wait_diagnostics(&self, path: &Path, since: Instant, timeout: Duration) -> (Vec<Diagnostic>, bool) {
        let uri = file_uri(path);
        let deadline = Instant::now() + timeout;
        loop {
            {
                let store = self.inner.diagnostics.lock().unwrap();
                if let Some(d) = store.get(&uri) {
                    if d.received_at >= since {
                        return (d.items.clone(), true);
                    }
                }
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let _ = tokio::time::timeout(deadline - now, self.inner.diag_notify.notified()).await;
        }
        let store = self.inner.diagnostics.lock().unwrap();
        (store.get(&uri).map(|d| d.items.clone()).unwrap_or_default(), false)
    }

    /// Current diagnostics for every document the server has reported on,
    /// keyed by file path.
    pub fn all_diagnostics(&self) -> Vec<(String, Vec<Diagnostic>)> {
        let store = self.inner.diagnostics.lock().unwrap();
        let mut out: Vec<(String, Vec<Diagnostic>)> = store
            .iter()
            .filter(|(_, d)| !d.items.is_empty())
            .map(|(u, d)| (uri_to_path(u), d.items.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub async fn shutdown(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(3), self.request("shutdown", serde_json::Value::Null)).await;
        let _ = self.notify("exit", serde_json::Value::Null).await;
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.start_kill();
        }
    }
}

pub fn uri_to_path(uri: &str) -> String {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = raw.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                decoded.push(v);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&decoded).to_string()
}

async fn read_loop(stdout: tokio::process::ChildStdout, inner: Arc<Inner>) {
    let mut reader = BufReader::new(stdout);
    loop {
        // Headers
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => return finish(&inner),
                Ok(_) => {}
                Err(_) => return finish(&inner),
            }
            let t = line.trim_end();
            if t.is_empty() {
                break;
            }
            if let Some(v) = t.strip_prefix("Content-Length:") {
                content_length = v.trim().parse().ok();
            }
        }
        let Some(len) = content_length else { continue };
        let mut body = vec![0u8; len];
        if reader.read_exact(&mut body).await.is_err() {
            return finish(&inner);
        }
        let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&body) else { continue };
        if let Some(id) = msg.get("id").and_then(|v| v.as_i64()) {
            if msg.get("method").is_none() {
                if let Some(tx) = inner.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(msg);
                }
                continue;
            }
            // A server→client request (workDoneProgress/create, configuration…):
            // answer with null so the server does not stall on us.
            let reply = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": serde_json::Value::Null });
            if let Ok(body) = serde_json::to_vec(&reply) {
                let mut stdin = inner.stdin.lock().await;
                let _ = stdin
                    .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
                    .await;
                let _ = stdin.write_all(&body).await;
                let _ = stdin.flush().await;
            }
            continue;
        }
        if msg.get("method").and_then(|m| m.as_str()) == Some("textDocument/publishDiagnostics") {
            if let Some(params) = msg.get("params") {
                if let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(params.clone()) {
                    inner.diagnostics.lock().unwrap().insert(
                        p.uri,
                        DocDiagnostics { version: p.version, received_at: Instant::now(), items: p.diagnostics },
                    );
                    inner.diag_notify.notify_waiters();
                }
            }
        }
    }
}

fn finish(inner: &Arc<Inner>) {
    inner.pending.lock().unwrap().clear();
    inner.diag_notify.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trip() {
        let p = Path::new("/tmp/my dir/a b.rs");
        let u = file_uri(p);
        assert_eq!(u, "file:///tmp/my%20dir/a%20b.rs");
        assert_eq!(uri_to_path(&u), "/tmp/my dir/a b.rs");
    }

    #[test]
    fn severity_labels() {
        let d = Diagnostic { range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 1 } }, severity: Some(1), code: None, source: None, message: "x".into() };
        assert_eq!(d.severity_label(), "error");
    }
}

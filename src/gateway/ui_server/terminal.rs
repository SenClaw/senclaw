//! Interactive terminal over WebSocket — spawns a local login shell in the
//! chat's workspace dir and bridges its PTY to the Flutter `xterm` widget.
//!
//! Protocol (binary-first):
//!   • server → client: raw PTY output as binary frames
//!   • client → server: binary frames = keystrokes written to the PTY;
//!     text frames = either keystrokes, or a JSON control `{"type":"resize",
//!     "cols":N,"rows":M}` to resize the PTY.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::Query;
use axum::response::Response;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::Deserialize;

use crate::util::paths::expand_tilde;

#[derive(Debug, Deserialize)]
pub(crate) struct TerminalQuery {
    /// Working directory for the shell (absolute or `~`). Falls back to $HOME.
    pub cwd: Option<String>,
}

/// GET /api/ws/terminal?cwd=... — upgrade to a PTY-backed shell session.
pub(crate) async fn ws_terminal(ws: WebSocketUpgrade, Query(q): Query<TerminalQuery>) -> Response {
    ws.on_upgrade(move |socket| handle(socket, q.cwd))
}

async fn handle(mut socket: WebSocket, cwd: Option<String>) {
    let pty = native_pty_system();
    let pair = match pty.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(_) => return,
    };

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let mut cmd = CommandBuilder::new(shell);
    cmd.env("TERM", "xterm-256color");
    if let Some(dir) = cwd.as_ref().map(|s| expand_tilde(s)) {
        if dir.is_dir() {
            cmd.cwd(dir);
        }
    }

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(_) => return,
    };
    // The slave is held open by the child; drop our handle so EOF propagates.
    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return,
    };
    let writer = match pair.master.take_writer() {
        Ok(w) => Arc::new(Mutex::new(w)),
        Err(_) => return,
    };

    // Blocking PTY reads happen on a dedicated thread; bytes flow back over a
    // channel so the async side can forward them to the socket.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(data) => {
                    if socket.send(Message::Binary(data)).await.is_err() {
                        break;
                    }
                }
                None => break, // shell exited
            },
            inc = socket.recv() => match inc {
                Some(Ok(Message::Binary(b))) => {
                    let mut w = writer.lock().unwrap();
                    if w.write_all(&b).and_then(|_| w.flush()).is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(t))) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        if v.get("type").and_then(|x| x.as_str()) == Some("resize") {
                            let cols = v.get("cols").and_then(|x| x.as_u64()).unwrap_or(80) as u16;
                            let rows = v.get("rows").and_then(|x| x.as_u64()).unwrap_or(24) as u16;
                            let _ = pair.master.resize(PtySize {
                                rows,
                                cols,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                            continue;
                        }
                    }
                    let mut w = writer.lock().unwrap();
                    let _ = w.write_all(t.as_bytes()).and_then(|_| w.flush());
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
        }
    }

    let _ = child.kill();
}

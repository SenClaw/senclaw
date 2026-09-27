//! Interactive terminal into a sandbox, over a WebSocket.
//!
//! Same frame protocol as the terminal in apps/code-ide, so the xterm.js glue
//! is identical: client → server is JSON text, `{"d":"keys"}` for input and
//! `{"r":[cols,rows]}` to resize; server → client is raw PTY bytes.
//!
//! The shell is started under exactly the same confinement as a scripted run —
//! Seatbelt profile, bwrap namespaces, or `docker exec` into the container. A
//! terminal that skipped the sandbox would be a way around every limit the rest
//! of the app enforces.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::Value;

use crate::sandbox::backend::{self, direct};
use crate::sandbox::caps::{self, DirectKind};
use crate::sandbox::config;
use crate::sandbox::db::Sandbox;
use crate::sandbox::state::AppState;

pub async fn terminal_ws(
    State(s): State<AppState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let sb = match s.db.sandbox(&id) {
        Ok(sb) => sb,
        // The socket is accepted and then closed with the reason, rather than
        // rejected: a bare handshake failure gives the UI nothing to show.
        Err(e) => {
            let msg = e.to_string();
            return ws.on_upgrade(move |mut sock| async move {
                let _ = sock
                    .send(Message::Text(format!("\r\n[sandbox] {msg}\r\n")))
                    .await;
            });
        }
    };

    // Direct-only probe — opening a terminal must not wait on Docker.
    let kind = caps::direct_caps(false).await.kind;
    // Docker terminals need the container up; starting it here means opening a
    // terminal on a stopped sandbox just works.
    if sb.backend == "docker" {
        if let Err(e) = crate::sandbox::runner::ensure_started(&s.db, &sb).await {
            let msg = e.to_string();
            return ws.on_upgrade(move |mut sock| async move {
                let _ = sock
                    .send(Message::Text(format!(
                        "\r\n[sandbox] cannot start the container: {msg}\r\n"
                    )))
                    .await;
            });
        }
    }

    let allowlist = crate::sandbox::settings::load(&s.db).allowlist;
    ws.on_upgrade(move |socket| handle(socket, sb, kind, allowlist))
}

/// Build the command that opens an interactive shell inside the sandbox.
pub fn shell_command(
    sb: &Sandbox,
    kind: DirectKind,
    allowlist: &[String],
) -> Result<CommandBuilder, String> {
    if sb.backend == "docker" {
        let mut cmd = CommandBuilder::new(config::docker_bin());
        cmd.arg("exec");
        cmd.arg("-it");
        cmd.arg("-w");
        cmd.arg(backend::docker::WORK);
        cmd.arg(format!("senclaw-sbx-{}", sb.id));
        cmd.arg("sh");
        cmd.arg("-l");
        return Ok(cmd);
    }

    match kind {
        DirectKind::Seatbelt => {
            let profile = direct::write_seatbelt_profile(sb, allowlist)?;
            let mut cmd = CommandBuilder::new("/usr/bin/sandbox-exec");
            cmd.arg("-f");
            cmd.arg(profile);
            cmd.arg("/bin/sh");
            Ok(cmd)
        }
        DirectKind::Bubblewrap => {
            let mut cmd = CommandBuilder::new("bwrap");
            // Reuse the scripted-run arguments, minus the trailing `-s` that
            // makes `sh` read a program from stdin — a terminal wants the
            // interactive shell instead.
            let args = direct::bwrap_args(
                &sb.workdir,
                &std::env::var("HOME").unwrap_or_default(),
                sb.network,
                &sb.mounts,
                sb.fs_mode,
                allowlist,
                &sb.ports,
            );
            for a in args.iter().filter(|a| a.as_str() != "-s") {
                cmd.arg(a);
            }
            Ok(cmd)
        }
        DirectKind::Degraded => Ok(CommandBuilder::new("/bin/sh")),
        DirectKind::AppContainer => Err(
            "an interactive terminal inside an AppContainer is not supported yet — use the Run tab, or the docker backend"
                .into(),
        ),
        DirectKind::Unsupported => {
            Err("this OS cannot run direct sandboxes — use the docker backend".into())
        }
    }
}

async fn handle(socket: WebSocket, sb: Sandbox, kind: DirectKind, allowlist: Vec<String>) {
    let (mut tx, mut rx) = socket.split();

    let mut cmd = match shell_command(&sb, kind, &allowlist) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Message::Text(format!("\r\n[sandbox] {e}\r\n"))).await;
            return;
        }
    };

    // Same rule as scripted runs: the child's environment is constructed, not
    // inherited, so the daemon's API keys never reach an interactive shell.
    cmd.env_clear();
    let home = if sb.backend == "docker" {
        backend::docker::WORK.to_string()
    } else {
        sb.workdir.clone()
    };
    for (k, v) in backend::build_env(&sb, &Default::default(), &home) {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    if sb.backend != "docker" {
        cmd.cwd(&sb.workdir);
    }

    let pair = match native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Message::Text(format!("\r\n[sandbox] pty: {e}\r\n"))).await;
            return;
        }
    };

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx
                .send(Message::Text(format!("\r\n[sandbox] cannot open a shell: {e}\r\n")))
                .await;
            return;
        }
    };
    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return,
    };
    let writer = Arc::new(Mutex::new(match pair.master.take_writer() {
        Ok(w) => w,
        Err(_) => return,
    }));
    let master = Arc::new(Mutex::new(pair.master));

    // PTY reads are blocking; they belong on the blocking pool, bridged to the
    // socket through a channel.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let pump = tokio::spawn(async move {
        while let Some(bytes) = out_rx.recv().await {
            if tx.send(Message::Binary(bytes)).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = rx.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(d) = v.get("d").and_then(|d| d.as_str()) {
            let _ = writer.lock().unwrap().write_all(d.as_bytes());
        } else if let Some(r) = v.get("r").and_then(|r| r.as_array()) {
            let cols = r.first().and_then(Value::as_u64).unwrap_or(80) as u16;
            let rows = r.get(1).and_then(Value::as_u64).unwrap_or(24) as u16;
            let _ = master.lock().unwrap().resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    let _ = child.kill();
    pump.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sb(backend: &str) -> Sandbox {
        Sandbox {
            id: "abc".into(),
            name: "n".into(),
            backend: backend.into(),
            image: Some("python:3.12-slim".into()),
            workdir: "/w".into(),
            network: false,
            cpus: 1.0,
            memory_mb: 512,
            pids_limit: 256,
            timeout_ms: 1000,
            env: json!({}),
            status: "running".into(),
            mounts: Vec::new(),
            fs_mode: crate::sandbox::fsmode::FsMode::Strict,
            trace_enabled: false,
            ports: Default::default(),
            container_id: None,
            last_error: None,
            created_at: 0,
            updated_at: 0,
            last_used_at: None,
        }
    }

    #[test]
    fn docker_terminal_execs_into_the_sandboxs_own_container() {
        let cmd = shell_command(&sb("docker"), DirectKind::Seatbelt, &[]).unwrap();
        let argv: Vec<String> = cmd
            .get_argv()
            .iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        assert!(argv.contains(&"senclaw-sbx-abc".to_string()));
        assert!(argv.contains(&"-it".to_string()));
    }

    #[test]
    fn bwrap_terminal_drops_the_stdin_script_flag() {
        let cmd = shell_command(&sb("direct"), DirectKind::Bubblewrap, &[]).unwrap();
        let argv: Vec<String> = cmd
            .get_argv()
            .iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect();
        assert!(
            !argv.iter().any(|a| a == "-s"),
            "`sh -s` would read the program from stdin instead of being interactive"
        );
        assert!(argv.iter().any(|a| a == "--unshare-net"));
    }

    #[test]
    fn an_unsupported_host_is_told_to_use_docker() {
        let e = shell_command(&sb("direct"), DirectKind::Unsupported, &[]).unwrap_err();
        assert!(e.contains("docker"));
    }
}

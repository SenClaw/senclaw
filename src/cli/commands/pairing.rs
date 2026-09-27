//! `senclaw pairing` — approve a chat from a terminal.
//!
//! The bot answers an unknown chat with a code and stops there. This is one of
//! the two places that code gets turned into access (the other is Settings →
//! Channels → Pairing).
//!
//! Talks to the running daemon over loopback rather than opening the DB: an
//! approval has to reach the waiting chat with "you're in", and the channel
//! connection lives in the daemon's process, not this one.

use anyhow::{bail, Context, Result};
use clap::Subcommand;

use crate::config::Config;

#[derive(Subcommand, Debug)]
pub enum PairingCmd {
    /// Show chats waiting to be let in
    List {
        /// Include already approved/rejected requests
        #[arg(long)]
        all: bool,
    },
    /// Approve a chat by the code the bot gave it
    Approve {
        /// The 8-character code (dashes and case are ignored)
        code: String,
    },
    /// Turn a request away, by request id (see `pairing list`)
    Reject { id: i64 },
}

/// Base URL of the local daemon, honouring `SENCLAW_UI_PORT`.
fn api_base(cfg: &Config) -> String {
    format!("http://127.0.0.1:{}", cfg.ui_server.port)
}

/// The daemon's token, if one is needed.
///
/// Under the default `auto` auth mode a loopback caller is exempt and this is
/// never checked; under `always` — the correct setting behind a same-host
/// reverse proxy — even this CLI must present it. Reading the file is what
/// makes the command keep working there without the user exporting anything.
fn auth_header(cfg: &Config) -> Option<String> {
    if let Some(t) = &cfg.ui_server.api_token {
        return Some(t.clone());
    }
    // Same file `auth.rs` writes: the token lives beside `config.json`.
    let dir = cfg.paths.global_config_path.parent()?;
    std::fs::read_to_string(dir.join("api_token"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn client(cfg: &Config) -> Result<(reqwest::Client, Option<String>)> {
    let c = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    Ok((c, auth_header(cfg)))
}

/// Turn a transport failure into the one thing the user can act on. A refused
/// connection here means the daemon is not running, which is a different fix
/// from a bad code and must not be reported as one.
fn explain_transport(e: reqwest::Error) -> anyhow::Error {
    if e.is_connect() {
        anyhow::anyhow!("không kết nối được daemon — SenClaw có đang chạy không?")
    } else {
        anyhow::Error::new(e)
    }
}

async fn send(
    cfg: &Config,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    let (c, token) = client(cfg)?;
    let mut req = c.request(method, format!("{}{path}", api_base(cfg)));
    if let Some(t) = token {
        req = req.header("X-SenClaw-Token", t);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.map_err(explain_transport)?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let parsed: Option<serde_json::Value> = serde_json::from_str(&text).ok();
    if !status.is_success() {
        // The daemon words its refusals for a person (expired code, already
        // handled, channel has no agent). Pass them through verbatim rather
        // than flattening all three into "request failed".
        let msg = parsed
            .as_ref()
            .and_then(|j| j.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or(text.trim());
        bail!("{}", if msg.is_empty() { status.as_str() } else { msg });
    }
    // A 200 that is not JSON is the daemon's SPA fallback answering a route it
    // does not have — i.e. a daemon older than this CLI. Reading the missing
    // body as an empty result printed "no chats waiting" at a daemon that had
    // never heard of pairing, which is worse than an error: the operator would
    // leave a real request sitting unapproved.
    parsed.ok_or_else(|| {
        anyhow::anyhow!(
            "daemon trả về nội dung không phải JSON cho {path} — nhiều khả năng daemon đang chạy cũ hơn CLI này. Khởi động lại daemon với bản build mới."
        )
    })
}

pub async fn run(cmd: PairingCmd) -> Result<()> {
    let cfg = Config::from_env();
    match cmd {
        PairingCmd::List { all } => {
            let path = if all {
                "/api/pairings?all=true"
            } else {
                "/api/pairings"
            };
            let json = send(&cfg, reqwest::Method::GET, path, None).await?;
            let items = json["pairings"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("Không có chat nào đang chờ duyệt.");
                return Ok(());
            }
            println!("{:<5} {:<10} {:<10} {:<22} {}", "ID", "CODE", "STATUS", "SENDER", "CHAT");
            for p in &items {
                println!(
                    "{:<5} {:<10} {:<10} {:<22} {}",
                    p["id"].as_i64().unwrap_or(0),
                    p["code"].as_str().unwrap_or(""),
                    p["status"].as_str().unwrap_or(""),
                    p["senderName"].as_str().unwrap_or("-"),
                    p["chatJid"].as_str().unwrap_or(""),
                );
            }
            println!("\nDuyệt:  senclaw pairing approve <CODE>");
        }
        PairingCmd::Approve { code } => {
            let json = send(
                &cfg,
                reqwest::Method::POST,
                "/api/pairings/approve-code",
                Some(serde_json::json!({ "code": code })),
            )
            .await
            .context("duyệt thất bại")?;
            println!(
                "✅ Đã duyệt {} → agent '{}'",
                json["chatJid"].as_str().unwrap_or(""),
                json["agentFolder"].as_str().unwrap_or("")
            );
        }
        PairingCmd::Reject { id } => {
            send(
                &cfg,
                reqwest::Method::POST,
                &format!("/api/pairings/{id}/reject"),
                None,
            )
            .await
            .context("từ chối thất bại")?;
            println!("Đã từ chối request {id}.");
        }
    }
    Ok(())
}

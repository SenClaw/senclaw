//! Send MCP server. Port target: src-old/mcp/send-server.ts
//!
//! Tools: send_message, send_file.
//! Relays messages to the main process via HTTP SendBridge (127.0.0.1:{port}).

use anyhow::{Context, Result};
use serde::Serialize;

use crate::db::Db;
use crate::mcp::schedule_server::ToolResult;

use rmcp::ServiceExt;

// ===== MCP stdio server =====

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct SendMessageParams {
    text: String,
    #[serde(default)]
    chat_jid: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct SendFileParams {
    file_path: String,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    chat_jid: Option<String>,
}

#[derive(Clone)]
pub struct McpSendServer {
    bridge_port: u16,
    own_chat_jid: String,
    bot_token: Option<String>,
    db_path: Option<String>,
}

impl McpSendServer {
    /// Build from the bridge + chat env pair, or `None` when either is absent.
    /// See [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        let (Ok(bridge_raw), Ok(chat_jid)) = (
            std::env::var("SENCLAW_SEND_BRIDGE_PORT"),
            std::env::var("SENCLAW_CHAT_JID"),
        ) else {
            return Ok(None);
        };
        Ok(Some(Self {
            bridge_port: bridge_raw
                .parse()
                .context("invalid SENCLAW_SEND_BRIDGE_PORT")?,
            own_chat_jid: chat_jid,
            bot_token: std::env::var("SENCLAW_BOT_TOKEN").ok(),
            db_path: std::env::var("SENCLAW_DB_PATH").ok(),
        }))
    }

    fn open_db(&self) -> Option<Db> {
        let path = self.db_path.as_ref()?;
        let mut cfg = crate::config::Config::from_env();
        cfg.paths.db_path = std::path::PathBuf::from(path);
        Db::open(&cfg).ok()
    }
}

/// Start the send MCP server over stdio.
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpSendServer::from_env()?
        .context("SENCLAW_SEND_BRIDGE_PORT / SENCLAW_CHAT_JID not set")?;

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpSendServer {
    #[rmcp::tool(description = "Send a text message to a chat")]
    async fn send_message(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            SendMessageParams,
        >,
    ) -> String {
        let srv = SendServer::new(
            self.bridge_port,
            &self.own_chat_jid,
            self.bot_token.as_deref(),
            self.open_db(),
        );
        srv.send_message(&p.text, p.chat_jid.as_deref())
            .await
            .content
    }

    #[rmcp::tool(description = "Send a file to a chat via HTTP bridge")]
    async fn send_file(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            SendFileParams,
        >,
    ) -> String {
        let srv = SendServer::new(
            self.bridge_port,
            &self.own_chat_jid,
            self.bot_token.as_deref(),
            self.open_db(),
        );
        srv.send_file(&p.file_path, p.caption.as_deref(), p.chat_jid.as_deref())
            .await
            .content
    }
}

#[derive(Debug, Clone, Serialize)]
struct SendPayload {
    #[serde(rename = "type")]
    payload_type: String,
    #[serde(rename = "chatJid")]
    chat_jid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "filePath")]
    file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "botToken")]
    bot_token: Option<String>,
}

pub struct SendServer {
    bridge_port: u16,
    own_chat_jid: String,
    bot_token: Option<String>,
    db: Option<Db>,
}

impl SendServer {
    pub fn new(
        bridge_port: u16,
        own_chat_jid: &str,
        bot_token: Option<&str>,
        db: Option<Db>,
    ) -> Self {
        Self {
            bridge_port,
            own_chat_jid: own_chat_jid.to_owned(),
            bot_token: bot_token.map(|s| s.to_owned()),
            db,
        }
    }

    fn bridge_url(&self) -> String {
        format!("http://127.0.0.1:{}/send", self.bridge_port)
    }

    /// Validate target JID. `None` = valid; `Some(msg)` = error.
    fn validate_target(&self, target_jid: &str) -> Option<String> {
        if target_jid == self.own_chat_jid {
            return None;
        }
        // Every chat is a full-privilege admin now — any registered group is a
        // valid send target (no self-only restriction).
        let db = self.db.as_ref()?;
        match db.get_group(target_jid) {
            Ok(Some(_)) => None,
            Ok(None) => Some(format!("Target {target_jid} is not in registered groups")),
            Err(e) => Some(format!("DB validation failed: {e}")),
        }
    }

    async fn post_to_bridge(&self, payload: &SendPayload) -> Result<(), String> {
        let client = reqwest::Client::new();
        let res = client
            .post(&self.bridge_url())
            .json(payload)
            .send()
            .await
            .map_err(|e| format!("SendBridge request failed: {e}"))?;

        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            let err_msg = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("error").cloned())
                .and_then(|v| v.as_str().map(|s| s.to_owned()))
                .unwrap_or(text);
            return Err(format!("SendBridge error ({status}): {err_msg}"));
        }
        Ok(())
    }

    // ===== send_message =====

    pub async fn send_message(&self, text: &str, chat_jid: Option<&str>) -> ToolResult {
        let target_jid = chat_jid.unwrap_or(&self.own_chat_jid);
        if let Some(err) = self.validate_target(target_jid) {
            return ToolResult::err(format!("{err}"));
        }

        // Egress gate — đường tool. Đường còn lại (reply) được gate trong lib.rs.
        // Xem docs/agent-security-hooks.md §3.1.1.
        if !crate::security::gate(target_jid, text) {
            return ToolResult::err(
                "Bị egress guard chặn: nội dung trùng lặp với tin nhắn đến, hoặc vượt giới \
                 hạn tần suất/số người nhận. Nếu đây là gửi hợp lệ, hãy diễn đạt lại nội \
                 dung hoặc báo quản trị viên."
                    .to_string(),
            );
        }

        let payload = SendPayload {
            payload_type: "message".into(),
            chat_jid: target_jid.to_owned(),
            text: Some(text.to_owned()),
            file_path: None,
            caption: None,
            bot_token: self.bot_token.clone(),
        };

        match self.post_to_bridge(&payload).await {
            Ok(()) => ToolResult::ok(format!("Message sent to {target_jid}")),
            Err(e) => ToolResult::err(format!("Send failed: {e}")),
        }
    }

    // ===== send_file =====

    pub async fn send_file(
        &self,
        file_path: &str,
        caption: Option<&str>,
        chat_jid: Option<&str>,
    ) -> ToolResult {
        let target_jid = chat_jid.unwrap_or(&self.own_chat_jid);
        if let Some(err) = self.validate_target(target_jid) {
            return ToolResult::err(format!("{err}"));
        }

        let payload = SendPayload {
            payload_type: "file".into(),
            chat_jid: target_jid.to_owned(),
            text: None,
            file_path: Some(file_path.to_owned()),
            caption: caption.map(|s| s.to_owned()),
            bot_token: self.bot_token.clone(),
        };

        match self.post_to_bridge(&payload).await {
            Ok(()) => ToolResult::ok(format!("File sent to {target_jid}")),
            Err(e) => ToolResult::err(format!("Send failed: {e}")),
        }
    }
}

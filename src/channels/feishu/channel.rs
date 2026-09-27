use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::channels::feishu_ws;
use crate::channels::{Channel, MessageCallback, MetadataCallback};
use crate::types::InlineButton;

use super::api::{call_api, fetch_bot_info, upload_file};
use super::helpers::{jid_to_chat_id, jid_to_receive_id_type, split_message};
use super::token::get_or_refresh_token;
use super::types::{AppEntry, CachedToken, DedupState, FeishuDomain};
use super::ws::process_ws_event;
use super::{APP_INIT_TIMEOUT_SECS, FEISHU_CARD_MAX_LEN};

// ===== FeishuChannel =====

pub struct FeishuChannel {
    default_app_id: String,
    default_app_secret: String,
    default_domain: FeishuDomain,
    http: reqwest::Client,
    apps: Mutex<HashMap<String, AppEntry>>,
    tokens: Arc<Mutex<HashMap<String, CachedToken>>>,
    handlers: Arc<RwLock<Vec<MessageCallback>>>,
    meta_handlers: Arc<RwLock<Vec<MetadataCallback>>>,
    connected: AtomicBool,
    dedup: Arc<Mutex<DedupState>>,
    sender_name_cache: Arc<Mutex<HashMap<String, (String, i64)>>>,
    ws_connections: Mutex<HashMap<String, feishu_ws::WsConnection>>,
}

impl FeishuChannel {
    pub fn new(app_id: String, app_secret: String, domain: Option<String>) -> Self {
        let domain = match domain.as_deref() {
            Some("lark") => FeishuDomain::Lark,
            Some("feishu") | None => FeishuDomain::Feishu,
            Some(other) => FeishuDomain::Custom(other.to_string()),
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest::Client::build");
        Self {
            default_app_id: app_id,
            default_app_secret: app_secret,
            default_domain: domain,
            http,
            apps: Mutex::new(HashMap::new()),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            handlers: Arc::new(RwLock::new(Vec::new())),
            meta_handlers: Arc::new(RwLock::new(Vec::new())),
            connected: AtomicBool::new(false),
            dedup: Arc::new(Mutex::new(DedupState::new())),
            sender_name_cache: Arc::new(Mutex::new(HashMap::new())),
            ws_connections: Mutex::new(HashMap::new()),
        }
    }

    /// Register and start a Feishu app (idempotent).
    pub async fn add_app(
        &self,
        app_id: &str,
        app_secret: &str,
        domain: Option<FeishuDomain>,
    ) -> Result<bool> {
        if app_id.is_empty() || app_secret.is_empty() {
            return Ok(false);
        }
        {
            let apps = self.apps.lock().await;
            if apps.contains_key(app_id) {
                return Ok(true);
            }
        }

        let domain = domain.unwrap_or_else(|| self.default_domain.clone());
        let base_url = domain.base_url().to_string();

        tracing::info!("[FeishuChannel] add_app: starting {app_id} (domain={domain:?})");

        // Fetch bot info (with timeout)
        let bot_info = tokio::time::timeout(Duration::from_secs(APP_INIT_TIMEOUT_SECS), async {
            let token =
                get_or_refresh_token(&self.http, &base_url, app_id, app_secret, &self.tokens)
                    .await?;
            fetch_bot_info(&self.http, &base_url, &token).await
        })
        .await
        .map_err(|_| anyhow::anyhow!("add_app timed out after {APP_INIT_TIMEOUT_SECS}s"))??;

        tracing::info!(
            "[FeishuChannel] Bot info OK for {app_id}: name=\"{}\", open_id={}",
            bot_info.name,
            bot_info.open_id
        );

        let bot_open_id = bot_info.open_id.clone();

        {
            let mut apps = self.apps.lock().await;
            apps.insert(
                app_id.to_string(),
                AppEntry {
                    base_url: base_url.clone(),
                    app_id: app_id.to_string(),
                    app_secret: app_secret.to_string(),
                },
            );
        }
        self.connected.store(true, Ordering::SeqCst);

        // Start WS event listener
        let app_id_c = app_id.to_string();
        let handlers = Arc::clone(&self.handlers);
        let dedup = Arc::clone(&self.dedup);
        let sender_cache = Arc::clone(&self.sender_name_cache);

        let fetcher = super::ws::MediaFetcher {
            http: self.http.clone(),
            base_url: base_url.clone(),
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            tokens: Arc::clone(&self.tokens),
        };
        let on_event = Arc::new(move |payload: Vec<u8>| {
            process_ws_event(
                &payload,
                &app_id_c,
                &bot_open_id,
                &handlers,
                &dedup,
                &sender_cache,
                Some(&fetcher),
            );
        });

        match feishu_ws::start_event_listener(
            &base_url,
            app_id,
            app_secret,
            self.http.clone(),
            on_event,
        )
        .await
        {
            Ok(conn) => {
                self.ws_connections
                    .lock()
                    .await
                    .insert(app_id.to_string(), conn);
                tracing::info!("[FeishuChannel] App {app_id} connected (REST + WS)");
            }
            Err(e) => {
                tracing::warn!(
                    "[FeishuChannel] App {app_id} connected (REST only; WS failed: {e})"
                );
            }
        }

        Ok(true)
    }

    async fn resolve_app(&self, bot_token: Option<&str>) -> Option<AppEntry> {
        let token = bot_token.unwrap_or(&self.default_app_id);
        let apps = self.apps.lock().await;
        apps.get(token).cloned()
    }

    async fn get_token(&self, app_id: &str, app_secret: &str, base_url: &str) -> Result<String> {
        get_or_refresh_token(&self.http, base_url, app_id, app_secret, &self.tokens).await
    }

    // ===== Sender name cache =====

}

#[async_trait]
impl Channel for FeishuChannel {
    fn id(&self) -> &'static str {
        "feishu"
    }

    async fn connect(&self) -> Result<()> {
        if self.connected.load(Ordering::SeqCst) {
            return Ok(());
        }
        if self.default_app_id.is_empty() || self.default_app_secret.is_empty() {
            tracing::warn!("[FeishuChannel] No app credentials configured, disabled");
            return Ok(());
        }
        self.add_app(
            &self.default_app_id,
            &self.default_app_secret,
            Some(self.default_domain.clone()),
        )
        .await?;
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        // Shut down WS listeners
        let ws_conns: Vec<feishu_ws::WsConnection> = {
            let mut guard = self.ws_connections.lock().await;
            std::mem::take(&mut *guard).into_values().collect()
        };
        for conn in ws_conns {
            conn.shutdown();
        }
        self.apps.lock().await.clear();
        self.tokens.lock().await.clear();
        self.sender_name_cache.lock().await.clear();
        self.connected.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    async fn send_message(
        &self,
        chat_jid: &str,
        text: &str,
        bot_token: Option<&str>,
    ) -> Result<()> {
        let entry = self
            .resolve_app(bot_token)
            .await
            .ok_or_else(|| anyhow::anyhow!("Feishu app not found for JID: {chat_jid}"))?;
        let chat_id = jid_to_chat_id(chat_jid)
            .ok_or_else(|| anyhow::anyhow!("Invalid Feishu JID: {chat_jid}"))?;
        let receive_id_type = jid_to_receive_id_type(chat_jid);
        let token = self
            .get_token(&entry.app_id, &entry.app_secret, &entry.base_url)
            .await?;

        for part in split_message(text) {
            let content = serde_json::json!({"text": part}).to_string();
            call_api(
                &self.http,
                &entry.base_url,
                &token,
                "/open-apis/im/v1/messages",
                &super::types::SendMessageBody {
                    receive_id: chat_id.to_string(),
                    msg_type: "text".to_string(),
                    content,
                },
                receive_id_type,
            )
            .await?;
        }
        Ok(())
    }

    async fn send_file(
        &self,
        chat_jid: &str,
        file_path: &str,
        caption: Option<&str>,
        bot_token: Option<&str>,
    ) -> Result<()> {
        let entry = self
            .resolve_app(bot_token)
            .await
            .ok_or_else(|| anyhow::anyhow!("Feishu app not found for JID: {chat_jid}"))?;
        let chat_id = jid_to_chat_id(chat_jid)
            .ok_or_else(|| anyhow::anyhow!("Invalid Feishu JID: {chat_jid}"))?;
        let receive_id_type = jid_to_receive_id_type(chat_jid);
        let token = self
            .get_token(&entry.app_id, &entry.app_secret, &entry.base_url)
            .await?;

        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");

        let file_key =
            upload_file(&self.http, &entry.base_url, &token, file_path, file_name).await?;

        let content = serde_json::json!({"file_key": file_key}).to_string();
        call_api(
            &self.http,
            &entry.base_url,
            &token,
            "/open-apis/im/v1/messages",
            &super::types::SendMessageBody {
                receive_id: chat_id.to_string(),
                msg_type: "file".to_string(),
                content,
            },
            receive_id_type,
        )
        .await?;

        if let Some(cap) = caption {
            self.send_message(chat_jid, cap, bot_token).await?;
        }
        Ok(())
    }

    fn owns_jid(&self, chat_jid: &str) -> bool {
        chat_jid.starts_with("feishu:")
    }

    fn on_message(&self, handler: MessageCallback) {
        if let Ok(mut guard) = self.handlers.write() {
            guard.push(handler);
        }
    }

    fn on_metadata(&self, handler: MetadataCallback) {
        if let Ok(mut guard) = self.meta_handlers.write() {
            guard.push(handler);
        }
    }

    fn get_bot_username(&self, _bot_token: Option<&str>) -> Option<String> {
        // Can't block on async; caller should use after connect
        None
    }

    async fn send_with_buttons(
        &self,
        chat_jid: &str,
        text: &str,
        buttons: &[InlineButton],
        bot_token: Option<&str>,
    ) -> Result<()> {
        let entry = self
            .resolve_app(bot_token)
            .await
            .ok_or_else(|| anyhow::anyhow!("Feishu app not found for JID: {chat_jid}"))?;
        let chat_id = jid_to_chat_id(chat_jid)
            .ok_or_else(|| anyhow::anyhow!("Invalid Feishu JID: {chat_jid}"))?;
        let receive_id_type = jid_to_receive_id_type(chat_jid);
        let token = self
            .get_token(&entry.app_id, &entry.app_secret, &entry.base_url)
            .await?;

        let truncated = if text.len() > FEISHU_CARD_MAX_LEN {
            format!("{}…(content truncated)", &text[..FEISHU_CARD_MAX_LEN])
        } else {
            text.to_string()
        };

        let actions: Vec<serde_json::Value> = buttons
            .iter()
            .map(|btn| {
                let btn_type =
                    if btn.callback_data.contains("refuse") || btn.callback_data.contains("deny") {
                        "danger"
                    } else {
                        "primary"
                    };
                serde_json::json!({
                    "tag": "button",
                    "text": { "tag": "plain_text", "content": btn.label },
                    "type": btn_type,
                    "value": { "action": btn.callback_data },
                })
            })
            .collect();

        let card = serde_json::json!({
            "config": { "wide_screen_mode": true },
            "elements": [
                { "tag": "markdown", "content": truncated },
                { "tag": "action", "actions": actions },
            ],
        });

        call_api(
            &self.http,
            &entry.base_url,
            &token,
            "/open-apis/im/v1/messages",
            &super::types::SendMessageBody {
                receive_id: chat_id.to_string(),
                msg_type: "interactive".to_string(),
                content: card.to_string(),
            },
            receive_id_type,
        )
        .await?;
        Ok(())
    }
}

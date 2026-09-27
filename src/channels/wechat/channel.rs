//! WeChat iLink Bot channel adapter. Mirrors `src-old/channels/wechat.ts`.
//!
//! Protocol: Tencent WeChat official iLink Bot HTTP API.
//! Message transport: HTTP long polling (getUpdates, server hold up to 35s).
//! Authentication: Bearer ilink_bot_token.
//!
//! JID format: `wx:user:{ilink_user_id}` (1:1 chats only; group_id reserved but empty).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::channels::{Channel, MessageCallback, MetadataCallback};
use crate::types::{ChatType, IncomingMessage, InlineButton};

use super::api::run_qr_login;
use super::helpers::PendingMenuEntry;
use super::helpers::{
    extract_text, jid_to_user_id, load_account, load_context_tokens, load_sync_buf,
    markdown_to_plain, random_hex, random_wechat_uin, save_account, save_context_tokens,
    save_sync_buf, split_text, user_id_to_jid, with_api_headers, BACKOFF_DELAY_SECS,
    DEFAULT_API_TIMEOUT_MS, DEFAULT_BASE_URL, DEFAULT_LONG_POLL_TIMEOUT_MS, ITEM_TYPE_FILE,
    ITEM_TYPE_IMAGE, ITEM_TYPE_TEXT, ITEM_TYPE_VIDEO, MAX_CONSECUTIVE_FAILURES, MENU_TTL_SECS,
    MSG_STATE_FINISH, MSG_TYPE_BOT, MSG_TYPE_USER, RETRY_DELAY_SECS, SESSION_EXPIRED_ERRCODE,
    WECHAT_MAX_LEN,
};
use super::media::{
    download_attachments, upload_to_cdn, MAX_FILE_BYTES, UPLOAD_MEDIA_FILE, UPLOAD_MEDIA_IMAGE,
    UPLOAD_MEDIA_VIDEO,
};
use super::types::{
    GetUpdatesResponse, MessageItem, OutCdnMedia, OutFileItem, OutImageItem, OutVideoItem,
    SendMessageMsg, SendMessageRequest, TextItem, WeixinAccountData,
};
use crate::channels::media::mime_for_name;

// ===== WeChatChannel =====

pub struct WeChatChannel {
    account_id: String,
    api_base_url: String,
    token: Mutex<Option<String>>,
    base_url: Mutex<String>,
    context_tokens: Mutex<HashMap<String, String>>,
    get_updates_buf: Mutex<String>,
    connected: AtomicBool,
    shutdown: AtomicBool,
    http: reqwest::Client,
    handlers: Arc<RwLock<Vec<MessageCallback>>>,
    meta_handlers: Arc<RwLock<Vec<MetadataCallback>>>,
    menu_queues: Arc<Mutex<HashMap<String, Vec<PendingMenuEntry>>>>,
}

impl WeChatChannel {
    pub fn new(account_id: String, api_base_url: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest::Client::build");

        Self {
            account_id: account_id.clone(),
            api_base_url: api_base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            token: Mutex::new(None),
            base_url: Mutex::new(DEFAULT_BASE_URL.to_string()),
            context_tokens: Mutex::new(load_context_tokens(&account_id)),
            get_updates_buf: Mutex::new(load_sync_buf(&account_id)),
            connected: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            http,
            handlers: Arc::new(RwLock::new(Vec::new())),
            meta_handlers: Arc::new(RwLock::new(Vec::new())),
            menu_queues: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn get_owner_jid(&self) -> Option<String> {
        let account = load_account(&self.account_id)?;
        let uid = account.user_id?.trim().to_string();
        if uid.is_empty() {
            None
        } else {
            Some(user_id_to_jid(&uid))
        }
    }

    pub async fn start_polling(self: Arc<Self>) {
        let mut next_timeout_ms = DEFAULT_LONG_POLL_TIMEOUT_MS;
        let mut consecutive_failures: u32 = 0;

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }

            match self.do_poll(next_timeout_ms).await {
                Ok(resp) => {
                    if let Some(t) = resp.longpolling_timeout_ms {
                        if t > 0 {
                            next_timeout_ms = t;
                        }
                    }

                    let is_api_error = resp.ret.map_or(false, |r| r != 0)
                        || resp.errcode.map_or(false, |e| e != 0);

                    if is_api_error {
                        if resp.errcode == Some(SESSION_EXPIRED_ERRCODE)
                            || resp.ret == Some(SESSION_EXPIRED_ERRCODE)
                        {
                            tracing::error!(
                                "[WeChatChannel] Session expired, QR login required again"
                            );
                            self.connected.store(false, Ordering::SeqCst);
                            return;
                        }
                        consecutive_failures += 1;
                        tracing::error!(
                            "[WeChatChannel] getUpdates error ret={:?} errcode={:?} errmsg={:?} ({consecutive_failures}/{})",
                            resp.ret,
                            resp.errcode,
                            resp.errmsg,
                            MAX_CONSECUTIVE_FAILURES
                        );
                        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                            consecutive_failures = 0;
                            tokio::time::sleep(Duration::from_secs(BACKOFF_DELAY_SECS)).await;
                        } else {
                            tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                        }
                        continue;
                    }

                    consecutive_failures = 0;

                    // Update cursor
                    if let Some(ref buf) = resp.get_updates_buf {
                        *self.get_updates_buf.lock().await = buf.clone();
                        save_sync_buf(&self.account_id, buf);
                    }

                    // Process messages
                    if let Some(ref msgs) = resp.msgs {
                        for msg in msgs {
                            if msg.message_type != Some(MSG_TYPE_USER) {
                                continue;
                            }
                            let Some(ref from_user_id) = msg.from_user_id else {
                                continue;
                            };

                            // Cache context token
                            if let Some(ref ctx) = msg.context_token {
                                let mut tokens = self.context_tokens.lock().await;
                                tokens.insert(from_user_id.clone(), ctx.clone());
                                drop(tokens);
                                save_context_tokens(
                                    &self.account_id,
                                    &*self.context_tokens.lock().await,
                                );
                            }

                            let mut text = extract_text(msg.item_list.as_deref());
                            let attachments =
                                download_attachments(&self.http, msg.item_list.as_deref()).await;
                            // `extract_text` labels a media-only message ("[Image]",
                            // "[File]"); once the blob is attached the label would
                            // read as the user typing it, so it becomes the same
                            // caption-less marker Telegram uses.
                            if !attachments.is_empty() && text.starts_with('[') && text.ends_with(']')
                            {
                                text = "[Ảnh đính kèm]".to_string();
                            }
                            let chat_jid = user_id_to_jid(from_user_id);
                            let sender_name = from_user_id.split('@').next().unwrap_or("unknown");

                            // Numeric menu
                            if self.try_handle_menu(&chat_jid, &text).await {
                                continue;
                            }

                            let ts = msg.create_time_ms.map_or_else(
                                || {
                                    chrono::Utc::now()
                                        .format("%Y-%m-%dT%H:%M:%S.000Z")
                                        .to_string()
                                },
                                |ms| {
                                    let secs = ms / 1000;
                                    let ns = ((ms % 1000) * 1_000_000) as u32;
                                    chrono::DateTime::from_timestamp(secs, ns)
                                        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
                                        .unwrap_or_default()
                                },
                            );

                            let incoming = IncomingMessage {
                                id: msg.message_id.map(|mid| mid.to_string()).unwrap_or_else(
                                    || {
                                        format!(
                                            "{from_user_id}-{}",
                                            std::time::UNIX_EPOCH
                                                .elapsed()
                                                .unwrap_or_default()
                                                .as_millis()
                                        )
                                    },
                                ),
                                chat_jid: chat_jid.clone(),
                                sender_jid: chat_jid.clone(),
                                sender_name: sender_name.to_string(),
                                content: text,
                                timestamp: ts,
                                is_from_me: false,
                                chat_type: ChatType::Private,
                                mentions_bot_username: Some(false),
                                bot_token: Some(self.account_id.clone()),
                                native_msg_id: None,
                                attachments,
                            };

                            if let Ok(guard) = self.handlers.read() {
                                for h in guard.iter() {
                                    h(incoming.clone());
                                }
                            }
                        }
                    }
                }
                Err(_e) => {
                    if self.shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    consecutive_failures += 1;
                    tracing::error!(
                        "[WeChatChannel] getUpdates exception ({consecutive_failures}/{})",
                        MAX_CONSECUTIVE_FAILURES
                    );
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        consecutive_failures = 0;
                        tokio::time::sleep(Duration::from_secs(BACKOFF_DELAY_SECS)).await;
                    } else {
                        tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                    }
                }
            }
        }
    }

    async fn do_poll(&self, timeout_ms: u64) -> Result<GetUpdatesResponse> {
        let base_url = {
            let guard = self.base_url.lock().await;
            guard.clone()
        };
        let token = {
            let guard = self.token.lock().await;
            guard.clone()
        };
        let buf = {
            let guard = self.get_updates_buf.lock().await;
            guard.clone()
        };

        let url = format!("{base_url}/ilink/bot/getupdates");
        let body = serde_json::json!({"get_updates_buf": buf}).to_string();

        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Content-Length", body.len())
            .header("AuthorizationType", "ilink_bot_token")
            .header("X-WECHAT-UIN", random_wechat_uin())
            .body(body)
            .timeout(Duration::from_millis(timeout_ms + 5_000));

        if let Some(ref t) = token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }

        let resp = req.send().await.context("getUpdates request")?;
        let text = resp.text().await.context("getUpdates response body")?;
        serde_json::from_str(&text).with_context(|| format!("parse getUpdates: {text}"))
    }

    /// Everything a send needs: the bot token, the peer's user id, the
    /// `context_token` of their last message (without which the API refuses
    /// to deliver), and the API base URL. `None` when the chat has never
    /// spoken to us — there is nothing to reply into.
    async fn send_target(&self, chat_jid: &str) -> Result<Option<SendTarget>> {
        let token = {
            let guard = self.token.lock().await;
            guard
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Not logged in"))?
        };
        let user_id = jid_to_user_id(chat_jid)
            .ok_or_else(|| anyhow::anyhow!("Invalid WeChat JID: {chat_jid}"))?
            .to_string();
        let context_token = {
            let tokens = self.context_tokens.lock().await;
            tokens.get(&user_id).cloned()
        };
        let Some(context_token) = context_token else {
            tracing::warn!("[WeChatChannel] No context_token for userId={user_id}, skipping send");
            return Ok(None);
        };
        let base_url = {
            let guard = self.base_url.lock().await;
            guard.clone()
        };
        Ok(Some(SendTarget {
            token,
            user_id,
            context_token,
            base_url,
        }))
    }

    /// One `sendmessage` call carrying `item_list`.
    async fn send_items(&self, target: &SendTarget, item_list: Vec<MessageItem>) -> Result<()> {
        let client_id = format!(
            "senclaw-{}-{}",
            std::time::UNIX_EPOCH
                .elapsed()
                .unwrap_or_default()
                .as_millis(),
            random_hex(4)
        );
        let body = SendMessageRequest {
            msg: SendMessageMsg {
                from_user_id: String::new(),
                to_user_id: target.user_id.clone(),
                client_id,
                message_type: MSG_TYPE_BOT,
                message_state: MSG_STATE_FINISH,
                item_list,
                context_token: Some(target.context_token.clone()),
            },
        };

        let url = format!("{}/ilink/bot/sendmessage", target.base_url);
        let body_str = serde_json::to_string(&body)?;

        let resp = with_api_headers(self.http.post(&url), &target.token)
            .header("Content-Length", body_str.len())
            .body(body_str)
            .timeout(Duration::from_millis(DEFAULT_API_TIMEOUT_MS))
            .send()
            .await
            .context("sendMessage request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendMessage HTTP {status}: {text}");
        }
        Ok(())
    }

    async fn send_message_internal(&self, chat_jid: &str, text: &str) -> Result<()> {
        let Some(target) = self.send_target(chat_jid).await? else {
            return Ok(());
        };
        let plain = markdown_to_plain(text);
        for chunk in split_text(&plain, WECHAT_MAX_LEN) {
            let item = MessageItem {
                item_type: ITEM_TYPE_TEXT,
                text_item: Some(TextItem { text: chunk }),
                ..Default::default()
            };
            self.send_items(&target, vec![item]).await?;
        }
        Ok(())
    }

    /// Encrypt + upload a local file and send it as an image, video or file
    /// item, whichever its MIME type implies.
    async fn send_file_internal(&self, chat_jid: &str, file_path: &str) -> Result<()> {
        let Some(target) = self.send_target(chat_jid).await? else {
            return Ok(());
        };
        let bytes = tokio::fs::read(file_path)
            .await
            .with_context(|| format!("read {file_path}"))?;
        if bytes.len() > MAX_FILE_BYTES {
            anyhow::bail!(
                "{file_path} is {} bytes, over the {MAX_FILE_BYTES} byte limit",
                bytes.len()
            );
        }
        let file_name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let mime = mime_for_name(&file_name);
        let media_type = if mime.starts_with("image/") {
            UPLOAD_MEDIA_IMAGE
        } else if mime.starts_with("video/") {
            UPLOAD_MEDIA_VIDEO
        } else {
            UPLOAD_MEDIA_FILE
        };

        let up = upload_to_cdn(
            &self.http,
            &target.base_url,
            &target.token,
            &target.user_id,
            media_type,
            &bytes,
        )
        .await?;
        let media = OutCdnMedia {
            encrypt_query_param: up.encrypt_query_param,
            aes_key: up.aes_key_b64,
            encrypt_type: 1,
        };
        let item = match media_type {
            UPLOAD_MEDIA_IMAGE => MessageItem {
                item_type: ITEM_TYPE_IMAGE,
                image_item: Some(OutImageItem {
                    media,
                    mid_size: up.cipher_size,
                }),
                ..Default::default()
            },
            UPLOAD_MEDIA_VIDEO => MessageItem {
                item_type: ITEM_TYPE_VIDEO,
                video_item: Some(OutVideoItem {
                    media,
                    video_size: up.cipher_size,
                }),
                ..Default::default()
            },
            _ => MessageItem {
                item_type: ITEM_TYPE_FILE,
                file_item: Some(OutFileItem {
                    media,
                    file_name,
                    len: up.raw_size.to_string(),
                }),
                ..Default::default()
            },
        };
        self.send_items(&target, vec![item]).await
    }

    async fn send_text_menu(
        &self,
        chat_jid: &str,
        text: &str,
        buttons: &[InlineButton],
    ) -> Result<()> {
        let numbered: Vec<String> = buttons
            .iter()
            .enumerate()
            .map(|(i, b)| format!("{}. {}", i + 1, b.label))
            .collect();
        let full_text = format!(
            "{text}\n\n{}\n\n(Reply with the number to choose)",
            numbered.join("\n")
        );

        {
            let mut queues = self.menu_queues.lock().await;
            queues
                .entry(chat_jid.to_string())
                .or_default()
                .push(PendingMenuEntry {
                    options: buttons.to_vec(),
                    created_at: Instant::now(),
                });
        }

        self.send_message_internal(chat_jid, &full_text).await
    }

    /// Start periodic menu expiry sweep (every 60 s).
    fn start_menu_cleanup(menu_queues: Arc<Mutex<HashMap<String, Vec<PendingMenuEntry>>>>) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                if let Ok(mut queues) = menu_queues.try_lock() {
                    let ttl = Duration::from_secs(MENU_TTL_SECS);
                    let now = Instant::now();
                    queues.retain(|_jid, entries| {
                        entries.retain(|e| now.duration_since(e.created_at) < ttl);
                        !entries.is_empty()
                    });
                }
            }
        });
    }

    async fn try_handle_menu(&self, chat_jid: &str, content: &str) -> bool {
        // Opportunistic sweep of expired entries for this jid
        {
            let mut queues = self.menu_queues.lock().await;
            if let Some(entries) = queues.get_mut(chat_jid) {
                let ttl = Duration::from_secs(MENU_TTL_SECS);
                let now = Instant::now();
                entries.retain(|e| now.duration_since(e.created_at) < ttl);
                if entries.is_empty() {
                    queues.remove(chat_jid);
                }
            }
        }

        let mut queues = self.menu_queues.lock().await;
        let queue = match queues.get_mut(chat_jid) {
            Some(q) if !q.is_empty() => q,
            _ => return false,
        };

        let num: Result<usize, _> = content.trim().parse();
        let Ok(num) = num else { return false };
        if num < 1 || num > queue[0].options.len() {
            return false;
        }

        let entry = queue.remove(0);
        if queue.is_empty() {
            queues.remove(chat_jid);
        }
        drop(queues);

        // iLink has no button/callback protocol, so a numbered menu is the
        // only way to offer choices and the chosen option's label is
        // delivered as the user's reply — the same thing they would have
        // typed had there been no menu.
        let selected = entry.options[num - 1].clone();
        let reply = IncomingMessage {
            id: format!("{chat_jid}-menu-{}", random_hex(4)),
            chat_jid: chat_jid.to_string(),
            sender_jid: chat_jid.to_string(),
            sender_name: jid_to_user_id(chat_jid)
                .and_then(|u| u.split('@').next())
                .unwrap_or("unknown")
                .to_string(),
            content: selected.label,
            timestamp: chrono::Utc::now().to_rfc3339(),
            is_from_me: false,
            chat_type: ChatType::Private,
            mentions_bot_username: Some(false),
            bot_token: Some(self.account_id.clone()),
            native_msg_id: None,
            attachments: Vec::new(),
        };
        if let Ok(guard) = self.handlers.read() {
            for h in guard.iter() {
                h(reply.clone());
            }
        }
        true
    }
}

/// Resolved destination for one outbound WeChat message.
struct SendTarget {
    token: String,
    user_id: String,
    context_token: String,
    base_url: String,
}

#[async_trait]
impl Channel for WeChatChannel {
    fn id(&self) -> &'static str {
        "wechat"
    }

    async fn connect(&self) -> Result<()> {
        if self.connected.load(Ordering::SeqCst) {
            return Ok(());
        }

        let account = load_account(&self.account_id);

        if account.as_ref().map_or(true, |a| a.token.is_empty()) {
            // QR login needed
            tracing::info!("[WeChatChannel] No saved credentials, starting QR login...");
            let result = run_qr_login(&self.http, &self.api_base_url).await?;
            *self.token.lock().await = Some(result.token.clone());
            *self.base_url.lock().await = result
                .base_url
                .clone()
                .unwrap_or_else(|| self.api_base_url.clone());

            let data = WeixinAccountData {
                token: result.token,
                base_url: result.base_url,
                user_id: result.user_id,
                saved_at: chrono::Utc::now().to_rfc3339(),
            };
            save_account(&self.account_id, &data);
            let _account = Some(data);
        } else {
            let acc = account.as_ref().unwrap();
            *self.token.lock().await = Some(acc.token.clone());
            *self.base_url.lock().await = acc
                .base_url
                .clone()
                .unwrap_or_else(|| self.api_base_url.clone());
            tracing::info!(
                "[WeChatChannel] Loaded credentials userId={:?}",
                acc.user_id
            );
        }

        // Restore state
        *self.get_updates_buf.lock().await = load_sync_buf(&self.account_id);
        *self.context_tokens.lock().await = load_context_tokens(&self.account_id);

        self.connected.store(true, Ordering::SeqCst);
        self.shutdown.store(false, Ordering::SeqCst);

        // Background polling is started by the daemon layer after connect() returns.

        // Start periodic menu expiry sweep
        Self::start_menu_cleanup(Arc::clone(&self.menu_queues));

        tracing::info!(
            "[WeChatChannel] Connected (accountId={} baseUrl={})",
            self.account_id,
            {
                let g = self.base_url.lock().await;
                g.clone()
            }
        );
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        self.connected.store(false, Ordering::SeqCst);
        tracing::info!("[WeChatChannel] Disconnected");
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    async fn send_message(
        &self,
        chat_jid: &str,
        text: &str,
        _bot_token: Option<&str>,
    ) -> Result<()> {
        self.send_message_internal(chat_jid, text).await
    }

    async fn send_file(
        &self,
        chat_jid: &str,
        file_path: &str,
        caption: Option<&str>,
        _bot_token: Option<&str>,
    ) -> Result<()> {
        self.send_file_internal(chat_jid, file_path).await?;
        if let Some(cap) = caption.filter(|c| !c.trim().is_empty()) {
            self.send_message_internal(chat_jid, cap).await?;
        }
        Ok(())
    }

    fn owns_jid(&self, chat_jid: &str) -> bool {
        if !chat_jid.starts_with("wx:") {
            return false;
        }
        // Multi-instance: only claim JIDs whose context_token we know
        // We can't block_on here, so check if jid matches known pattern
        true // simplified; full impl checks context_tokens
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

    async fn send_with_buttons(
        &self,
        chat_jid: &str,
        text: &str,
        buttons: &[InlineButton],
        _bot_token: Option<&str>,
    ) -> Result<()> {
        self.send_text_menu(chat_jid, text, buttons).await
    }
}

// Delegate Channel trait for Arc<WeChatChannel> so polling can own an Arc clone.
#[async_trait]
impl Channel for Arc<WeChatChannel> {
    fn id(&self) -> &'static str {
        self.as_ref().id()
    }
    async fn connect(&self) -> Result<()> {
        self.as_ref().connect().await
    }
    async fn disconnect(&self) -> Result<()> {
        self.as_ref().disconnect().await
    }
    fn is_connected(&self) -> bool {
        self.as_ref().is_connected()
    }
    async fn send_message(
        &self,
        chat_jid: &str,
        text: &str,
        bot_token: Option<&str>,
    ) -> Result<()> {
        self.as_ref().send_message(chat_jid, text, bot_token).await
    }
    async fn send_file(
        &self,
        chat_jid: &str,
        file_path: &str,
        caption: Option<&str>,
        bot_token: Option<&str>,
    ) -> Result<()> {
        self.as_ref()
            .send_file(chat_jid, file_path, caption, bot_token)
            .await
    }
    fn owns_jid(&self, chat_jid: &str) -> bool {
        self.as_ref().owns_jid(chat_jid)
    }
    fn on_message(&self, handler: MessageCallback) {
        self.as_ref().on_message(handler)
    }
    fn on_metadata(&self, handler: MetadataCallback) {
        self.as_ref().on_metadata(handler)
    }
    fn get_bot_username(&self, bot_token: Option<&str>) -> Option<String> {
        self.as_ref().get_bot_username(bot_token)
    }
    async fn send_with_buttons(
        &self,
        chat_jid: &str,
        text: &str,
        buttons: &[InlineButton],
        bot_token: Option<&str>,
    ) -> Result<()> {
        self.as_ref()
            .send_with_buttons(chat_jid, text, buttons, bot_token)
            .await
    }
    async fn set_typing(
        &self,
        chat_jid: &str,
        active: bool,
        bot_token: Option<&str>,
    ) -> Result<()> {
        self.as_ref().set_typing(chat_jid, active, bot_token).await
    }
}

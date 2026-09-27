//! Telegram channel adapter using teloxide. Mirrors `src-old/channels/telegram.ts`.
//!
//! JID format: `tg:{botUserId}:user:{chatId}` (private) or `tg:{botUserId}:group:{chatId}` (group).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use teloxide::payloads::SendMessageSetters;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, InputFile, Recipient, ReplyMarkup};
use tokio::sync::{oneshot, Mutex};

use crate::channels::{Channel, MessageCallback, MetadataCallback};
use crate::types::InlineButton;

const TG_MAX_LEN: usize = 4096;
const BOT_INIT_TIMEOUT_SECS: u64 = 15;
const TYPING_INTERVAL_SECS: u64 = 4;
/// Long-polling window sent to Telegram (seconds).
const POLL_TIMEOUT_SECS: u64 = 25;
/// HTTP client timeout — must be > POLL_TIMEOUT_SECS to avoid races.
const HTTP_TIMEOUT_SECS: u64 = 60;

// ===== Helpers =====

fn chat_id_to_jid(chat_id: i64, chat_type: &str, bot_user_id: u64) -> String {
    let suffix = if chat_type == "private" {
        format!("user:{chat_id}")
    } else {
        format!("group:{chat_id}")
    };
    format!("tg:{bot_user_id}:{suffix}")
}

/// Cap on a downloaded Telegram photo, before base64. Telegram itself serves
/// bot downloads up to 20 MB; this keeps a single message from turning into a
/// prompt no provider will accept.
const MAX_MEDIA_BYTES: u32 = 8 * 1024 * 1024;

/// Download one Telegram file into a `data:` URL.
///
/// Two steps: `getFile` resolves the opaque `file_id` to a path, then teloxide's
/// own `download_file` fetches it — which keeps the bot token out of a URL we'd
/// otherwise build (and risk logging) by hand.
///
/// Returns `None` on any failure. A photo we can't fetch has to degrade to a
/// text-only message; dropping the user's message instead would be worse.
async fn download_media(
    bot: &Bot,
    file_id: String,
    size: u32,
    mime_type: &str,
) -> Option<crate::types::MessageAttachment> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use teloxide::net::Download;

    if size > MAX_MEDIA_BYTES {
        tracing::warn!(
            "[TelegramChannel] skipping {size}-byte media, over the {MAX_MEDIA_BYTES} limit"
        );
        return None;
    }
    let file = match bot.get_file(file_id).send().await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("[TelegramChannel] getFile failed: {e}");
            return None;
        }
    };
    let mut bytes: Vec<u8> = Vec::with_capacity(size as usize);
    if let Err(e) = bot.download_file(&file.path, &mut bytes).await {
        tracing::warn!("[TelegramChannel] media download failed: {e}");
        return None;
    }
    tracing::info!(
        "[TelegramChannel] downloaded {} bytes of {mime_type} media",
        bytes.len()
    );
    Some(crate::types::MessageAttachment {
        data_url: format!("data:{mime_type};base64,{}", STANDARD.encode(&bytes)),
        mime_type: mime_type.to_string(),
        name: std::path::Path::new(&file.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string()),
    })
}

fn jid_to_chat_id(jid: &str) -> Option<i64> {
    let re = regex::Regex::new(r"^tg:(?:\d+:)?(?:user|group):(-?\d+)$").unwrap();
    re.captures(jid)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

fn split_message(text: &str) -> Vec<String> {
    if text.len() <= TG_MAX_LEN {
        return vec![text.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut remaining = text;
    while remaining.len() > TG_MAX_LEN {
        let chunk = &remaining[..TG_MAX_LEN];
        let split_at = chunk
            .rfind('\n')
            .filter(|&pos| pos > TG_MAX_LEN / 2)
            .unwrap_or(TG_MAX_LEN);
        parts.push(remaining[..split_at].to_string());
        remaining = remaining[split_at..].trim_start_matches('\n');
    }
    if !remaining.is_empty() {
        parts.push(remaining.to_string());
    }
    parts
}

fn check_mention(
    text: &str,
    entities: &[teloxide::types::MessageEntity],
    bot_username: &str,
) -> bool {
    let lower_bot = format!("@{}", bot_username.to_lowercase());
    for entity in entities {
        if entity.kind == teloxide::types::MessageEntityKind::Mention {
            let mention: String = text
                .chars()
                .skip(entity.offset)
                .take(entity.length)
                .collect();
            if mention.to_lowercase() == lower_bot {
                return true;
            }
        }
    }
    text.to_lowercase().contains(&lower_bot)
}

// ===== TelegramChannel =====

struct BotEntry {
    bot: Bot,
    username: String,
}

pub struct TelegramChannel {
    default_token: String,
    handlers: Arc<RwLock<Vec<MessageCallback>>>,
    meta_handlers: Arc<RwLock<Vec<MetadataCallback>>>,
    bots: Mutex<HashMap<String, BotEntry>>,
    /// Bot user ids this adapter actually holds, readable without `.await`.
    ///
    /// `bots` is behind a tokio mutex and [`Channel::owns_jid`] is sync, so the
    /// routing question "is this chat mine?" could not consult it — `owns_jid`
    /// answered `starts_with("tg:")` and every Telegram adapter claimed every
    /// Telegram chat. With more than one adapter (a tokenless default one plus
    /// one built from a DB channel) the send loop stopped at whichever came
    /// first and its `send_message` failed with "Bot not found".
    bot_user_ids: std::sync::RwLock<std::collections::HashSet<u64>>,
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    connected: AtomicBool,
    /// chat_jid → cancel token for typing indicator
    typing_cancels: Mutex<HashMap<String, oneshot::Sender<()>>>,
}

impl TelegramChannel {
    pub fn new(token: String) -> Self {
        Self {
            default_token: token,
            handlers: Arc::new(RwLock::new(Vec::new())),
            meta_handlers: Arc::new(RwLock::new(Vec::new())),
            bots: Mutex::new(HashMap::new()),
            bot_user_ids: std::sync::RwLock::new(std::collections::HashSet::new()),
            shutdown_tx: Mutex::new(None),
            connected: AtomicBool::new(false),
            typing_cancels: Mutex::new(HashMap::new()),
        }
    }

    pub fn get_bot_user_id(&self, bot_token: Option<&str>) -> Option<u64> {
        let _token = bot_token.unwrap_or(&self.default_token);
        // Can't easily access this without async — simplified for now
        None
    }

    /// Add an extra bot token (for multi-bot setups).
    pub async fn add_bot(&self, token: &str) -> Result<()> {
        if token.is_empty() {
            return Ok(());
        }
        {
            let bots = self.bots.lock().await;
            if bots.contains_key(token) {
                return Ok(());
            }
        }

        // Build a reqwest client whose timeout exceeds the long-poll window so
        // Telegram always returns before the HTTP layer closes the connection.
        // Use teloxide's own ClientBuilder to avoid reqwest version conflicts.
        let http_client = teloxide::net::default_reqwest_settings()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .unwrap_or_default();
        let bot = Bot::with_client(token, http_client);

        // Fetch bot info with timeout
        let me = tokio::time::timeout(Duration::from_secs(BOT_INIT_TIMEOUT_SECS), bot.get_me())
            .await
            .map_err(|_| anyhow::anyhow!("bot.init() timed out"))?
            .context("getMe failed")?;

        let username = me.username.clone().unwrap_or_default();
        let bot_user_id = me.id.0 as u64;

        {
            let mut bots = self.bots.lock().await;
            bots.insert(
                token.to_string(),
                BotEntry {
                    bot: bot.clone(),
                    username: username.clone(),
                },
            );
        }
        // Published before the listener starts: `owns_jid` gates the reply path,
        // and a bot that can receive must already be able to answer.
        if let Ok(mut ids) = self.bot_user_ids.write() {
            ids.insert(bot_user_id);
        }
        self.connected.store(true, Ordering::SeqCst);

        // Start listening in background
        let handlers = Arc::clone(&self.handlers);
        let meta_handlers = Arc::clone(&self.meta_handlers);
        let token_owned = token.to_string();
        let username_clone = username.clone();

        tokio::spawn(async move {
            listen_loop(
                bot,
                token_owned,
                username_clone,
                bot_user_id,
                handlers,
                meta_handlers,
            )
            .await;
        });

        tracing::info!("[TelegramChannel] Bot @{username} started");
        Ok(())
    }

    async fn resolve_bot(&self, bot_token: Option<&str>) -> Option<Bot> {
        let token = bot_token.unwrap_or(&self.default_token);
        let bots = self.bots.lock().await;
        bots.get(token).map(|e| e.bot.clone())
    }

}

#[async_trait]
impl Channel for TelegramChannel {
    fn id(&self) -> &'static str {
        "telegram"
    }

    async fn connect(&self) -> Result<()> {
        if self.connected.load(Ordering::SeqCst) {
            return Ok(());
        }
        if self.default_token.is_empty() {
            tracing::warn!("[TelegramChannel] No bot token configured, disabled");
            return Ok(());
        }
        self.add_bot(&self.default_token.clone()).await
    }

    async fn disconnect(&self) -> Result<()> {
        // Stop all typing indicators
        {
            let mut cancels = self.typing_cancels.lock().await;
            for (_, tx) in cancels.drain() {
                let _ = tx.send(());
            }
        }

        // Signal shutdown
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }

        self.bots.lock().await.clear();
        if let Ok(mut ids) = self.bot_user_ids.write() {
            ids.clear();
        }
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
        let bot = self
            .resolve_bot(bot_token)
            .await
            .ok_or_else(|| anyhow::anyhow!("Bot not found"))?;
        let chat_id = jid_to_chat_id(chat_jid)
            .ok_or_else(|| anyhow::anyhow!("Invalid Telegram JID: {chat_jid}"))?;

        let recipient = Recipient::Id(ChatId(chat_id));
        for part in split_message(text) {
            bot.send_message(recipient.clone(), part).await?;
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
        let bot = self
            .resolve_bot(bot_token)
            .await
            .ok_or_else(|| anyhow::anyhow!("Bot not found"))?;
        let chat_id = jid_to_chat_id(chat_jid)
            .ok_or_else(|| anyhow::anyhow!("Invalid Telegram JID: {chat_jid}"))?;

        let recipient = Recipient::Id(ChatId(chat_id));
        let file = InputFile::file(file_path);
        let mut req = bot.send_document(recipient, file);
        if let Some(cap) = caption {
            req = req.caption(cap);
        }
        req.await?;
        Ok(())
    }

    /// Does this adapter hold the bot that owns `chat_jid`?
    ///
    /// Not just "is it a Telegram jid". The daemon pushes a default adapter
    /// unconditionally — token and all — and then builds another per DB channel,
    /// so a prefix-only answer made a tokenless adapter claim chats belonging to
    /// a bot it had never heard of. The reply loop breaks on the first owner, so
    /// that adapter swallowed every outgoing message.
    fn owns_jid(&self, chat_jid: &str) -> bool {
        let Some(rest) = chat_jid.strip_prefix("tg:") else {
            return false;
        };
        let Ok(ids) = self.bot_user_ids.read() else {
            return false;
        };
        // Holding no bot means owning nothing — this is the case that was
        // silently eating replies.
        if ids.is_empty() {
            return false;
        }
        match rest.split(':').next().unwrap_or("").parse::<u64>() {
            Ok(bot_user_id) => ids.contains(&bot_user_id),
            // Legacy `tg:user:{id}` / `tg:group:{id}` carry no bot id. Any
            // adapter that actually has a bot can serve them.
            Err(_) => true,
        }
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

    fn get_bot_username(&self, bot_token: Option<&str>) -> Option<String> {
        // Use try_lock to avoid blocking; returns None if lock is contended.
        let token = bot_token.unwrap_or(&self.default_token);
        self.bots
            .try_lock()
            .ok()?
            .get(token)
            .map(|e| e.username.clone())
    }

    async fn send_with_buttons(
        &self,
        chat_jid: &str,
        text: &str,
        buttons: &[InlineButton],
        bot_token: Option<&str>,
    ) -> Result<()> {
        let bot = self
            .resolve_bot(bot_token)
            .await
            .ok_or_else(|| anyhow::anyhow!("Bot not found"))?;
        let chat_id = jid_to_chat_id(chat_jid)
            .ok_or_else(|| anyhow::anyhow!("Invalid Telegram JID: {chat_jid}"))?;

        let mut keyboard = teloxide::types::InlineKeyboardMarkup::default();
        for btn in buttons {
            let row = vec![teloxide::types::InlineKeyboardButton::callback(
                &btn.label,
                &btn.callback_data,
            )];
            keyboard = keyboard.append_row(row);
        }

        bot.send_message(Recipient::Id(ChatId(chat_id)), text)
            .reply_markup(ReplyMarkup::InlineKeyboard(keyboard))
            .await?;
        Ok(())
    }

    async fn set_typing(
        &self,
        chat_jid: &str,
        active: bool,
        bot_token: Option<&str>,
    ) -> Result<()> {
        // Cancel existing typing indicator
        {
            let mut cancels = self.typing_cancels.lock().await;
            if let Some(tx) = cancels.remove(chat_jid) {
                let _ = tx.send(());
            }
        }

        if !active {
            return Ok(());
        }

        let bot = match self.resolve_bot(bot_token).await {
            Some(b) => b,
            None => return Ok(()),
        };
        let chat_id = match jid_to_chat_id(chat_jid) {
            Some(id) => ChatId(id),
            None => return Ok(()),
        };

        let (tx, mut rx) = oneshot::channel();
        {
            let mut cancels = self.typing_cancels.lock().await;
            cancels.insert(chat_jid.to_string(), tx);
        }

        // Spawn typing indicator loop
        tokio::spawn(async move {
            loop {
                let _ = bot
                    .send_chat_action(Recipient::Id(chat_id), ChatAction::Typing)
                    .await;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(TYPING_INTERVAL_SECS)) => {},
                    _ = &mut rx => break,
                }
            }
        });

        Ok(())
    }
}

// ===== Background listener =====

async fn listen_loop(
    bot: Bot,
    token: String,
    bot_username: String,
    bot_user_id: u64,
    handlers: Arc<RwLock<Vec<MessageCallback>>>,
    meta_handlers: Arc<RwLock<Vec<MetadataCallback>>>,
) {
    use teloxide::types::{MediaKind, MessageKind, UpdateKind};

    tracing::info!(
        "[TelegramChannel] Listener started for @{bot_username} (botUserId={bot_user_id})"
    );

    let mut offset: i32 = 0;
    loop {
        let updates = match bot
            .get_updates()
            .offset(offset)
            .timeout(POLL_TIMEOUT_SECS as u32)
            .send()
            .await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("[TelegramChannel] getUpdates error for @{bot_username}: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        for update in updates {
            offset = offset.max(update.id.0 as i32 + 1);

            let tg_msg = match &update.kind {
                UpdateKind::Message(m) => m,
                // Ignore non-message updates (edits, callbacks, etc.)
                _ => continue,
            };

            // Text, or a photo/image-document with its caption. Anything else
            // (audio, video, stickers) is still skipped — nothing downstream
            // can read it.
            let MessageKind::Common(common) = &tg_msg.kind else {
                continue;
            };
            let (text, media) = match &common.media_kind {
                MediaKind::Text(t) => (t.text.clone(), None),
                MediaKind::Photo(p) => {
                    // Telegram ships several rescaled copies; the last is the
                    // largest, which is what a vision model or OCR wants.
                    let Some(largest) = p.photo.last() else {
                        continue;
                    };
                    (
                        p.caption.clone().unwrap_or_default(),
                        Some((
                            largest.file.id.clone(),
                            largest.file.size,
                            "image/jpeg".to_string(),
                        )),
                    )
                }
                // An image sent "as file" arrives as a Document, not a Photo —
                // uncompressed, which is the better source when it is one.
                MediaKind::Document(d)
                    if d.document
                        .mime_type
                        .as_ref()
                        .is_some_and(|m| m.type_() == "image") =>
                {
                    let mime = d
                        .document
                        .mime_type
                        .as_ref()
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| "image/jpeg".to_string());
                    (
                        d.caption.clone().unwrap_or_default(),
                        Some((d.document.file.id.clone(), d.document.file.size, mime)),
                    )
                }
                _ => continue,
            };

            let attachments = match media {
                Some((file_id, size, mime)) => {
                    match download_media(&bot, file_id, size, &mime).await {
                        Some(a) => vec![a],
                        // Download failed: carry on with the caption alone
                        // rather than swallowing the user's message entirely.
                        None => Vec::new(),
                    }
                }
                None => Vec::new(),
            };

            // A caption-less photo would otherwise reach the agent as an empty
            // turn, which reads as "the user said nothing".
            let text = if text.trim().is_empty() && !attachments.is_empty() {
                "[Ảnh đính kèm]".to_string()
            } else {
                text
            };

            let chat_id = tg_msg.chat.id.0;
            let chat_type_str = match tg_msg.chat.kind {
                teloxide::types::ChatKind::Private(_) => "private",
                _ => "group",
            };
            let chat_jid = chat_id_to_jid(chat_id, chat_type_str, bot_user_id);

            let sender = tg_msg.from.as_ref();
            let sender_name = sender
                .map(|u| {
                    let mut n = u.first_name.clone();
                    if let Some(last) = &u.last_name {
                        n.push(' ');
                        n.push_str(last);
                    }
                    n
                })
                .unwrap_or_default();
            let sender_id = sender.map(|u| u.id.0).unwrap_or(0);
            let sender_jid = format!("tg:{bot_user_id}:user:{sender_id}");

            // Emit chat metadata on first sight
            {
                let title = tg_msg.chat.title().map(str::to_owned);
                let chat_type_enum = if chat_type_str == "private" {
                    crate::types::ChatType::Private
                } else {
                    crate::types::ChatType::Group
                };
                let meta = crate::types::ChatMeta {
                    jid: chat_jid.clone(),
                    title,
                    chat_type: chat_type_enum.clone(),
                };
                if let Ok(guard) = meta_handlers.read() {
                    for cb in guard.iter() {
                        cb(meta.clone());
                    }
                }
            }

            // Mention detection — only text messages carry entities.
            let entities = match &common.media_kind {
                MediaKind::Text(t) => t.entities.clone(),
                _ => vec![],
            };
            let mentions_bot = check_mention(&text, &entities, &bot_username);

            let chat_type_enum = if chat_type_str == "private" {
                crate::types::ChatType::Private
            } else {
                crate::types::ChatType::Group
            };

            let incoming = crate::types::IncomingMessage {
                id: tg_msg.id.to_string(),
                chat_jid: chat_jid.clone(),
                sender_name,
                sender_jid,
                content: text,
                timestamp: tg_msg.date.format("%Y-%m-%dT%H:%M:%S.000Z").to_string(),
                is_from_me: false,
                chat_type: chat_type_enum,
                mentions_bot_username: Some(mentions_bot),
                bot_token: Some(token.clone()),
                native_msg_id: Some(tg_msg.id.to_string()),
                attachments,
            };

            tracing::debug!(
                "[TelegramChannel] Message from {chat_jid}: \"{}\"",
                &incoming.content.chars().take(60).collect::<String>()
            );

            if let Ok(guard) = handlers.read() {
                for cb in guard.iter() {
                    cb(incoming.clone());
                }
            }
        }
    }
}

// ===== Tests =====

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jid_roundtrip() {
        let jid = chat_id_to_jid(123456789, "private", 987654321);
        assert_eq!(jid, "tg:987654321:user:123456789");
        assert_eq!(jid_to_chat_id(&jid), Some(123456789));
    }

    #[test]
    fn test_group_jid() {
        let jid = chat_id_to_jid(-1001234567890, "group", 555);
        assert_eq!(jid, "tg:555:group:-1001234567890");
        assert_eq!(jid_to_chat_id(&jid), Some(-1001234567890));
    }

    #[test]
    fn test_old_format_jid() {
        assert_eq!(jid_to_chat_id("tg:user:12345"), Some(12345));
        assert_eq!(jid_to_chat_id("tg:group:-67890"), Some(-67890));
    }

    #[test]
    fn test_split_short() {
        let parts = split_message("hello");
        assert_eq!(parts, vec!["hello"]);
    }

    #[test]
    fn test_split_long() {
        let long = "x".repeat(5000);
        let parts = split_message(&long);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), TG_MAX_LEN);
        assert_eq!(parts[1].len(), 5000 - TG_MAX_LEN);
    }

    #[test]
    fn test_split_at_newline() {
        let mut text = "x".repeat(3000);
        text.push('\n');
        text.push_str(&"y".repeat(2000));
        let parts = split_message(&text);
        assert_eq!(parts.len(), 2);
        // split_at is at the newline position; newline trimmed from part 2 start
        assert_eq!(parts[0].len(), 3000);
        assert_eq!(parts[1].len(), 2000);
    }

    /// Register a bot id without touching the network, so ownership can be
    /// tested without a live Telegram token.
    fn with_bot(token: &str, bot_user_id: u64) -> TelegramChannel {
        let ch = TelegramChannel::new(token.into());
        ch.bot_user_ids.write().unwrap().insert(bot_user_id);
        ch
    }

    #[test]
    fn test_owns_jid() {
        let ch = with_bot("test_token", 123);
        assert!(ch.owns_jid("tg:123:user:456"));
        assert!(!ch.owns_jid("feishu:user:abc"));
        assert!(!ch.owns_jid("wx:user:xyz"));
    }

    #[test]
    fn an_adapter_holding_no_bot_owns_nothing() {
        // The daemon pushes a default adapter unconditionally, even with an
        // empty token that never connects. Claiming `tg:` chats anyway put it
        // first in the reply loop, where `send_message` failed with "Bot not
        // found" and the error was discarded — every reply silently vanished.
        let dead = TelegramChannel::new(String::new());
        assert!(!dead.owns_jid("tg:8782092920:user:7848451981"));
        assert!(!dead.owns_jid("tg:user:456"));
    }

    #[test]
    fn an_adapter_does_not_claim_another_bots_chat() {
        let ch = with_bot("bot-A-token", 111);
        assert!(ch.owns_jid("tg:111:group:-100200"));
        assert!(
            !ch.owns_jid("tg:222:user:456"),
            "a chat belonging to bot 222 must fall through to the adapter that has it"
        );
    }

    #[test]
    fn a_bot_less_jid_is_served_by_any_adapter_that_has_a_bot() {
        // Legacy `tg:user:{id}` predates bot-aware jids and carries no bot id;
        // refusing it would strand old bindings.
        let ch = with_bot("tok", 999);
        assert!(ch.owns_jid("tg:user:456"));
        assert!(ch.owns_jid("tg:group:-100"));
    }
}

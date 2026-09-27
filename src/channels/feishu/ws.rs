use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::Mutex;

use crate::channels::MessageCallback;
use crate::types::{ChatType, IncomingMessage};

use super::api::download_message_resource;
use super::helpers::{
    check_bot_mention, make_jid, parse_media_refs, parse_text_content,
    remove_bot_mention_placeholders,
};
use super::token::get_or_refresh_token;
use super::types::{CachedToken, DedupState, WsEventEnvelope, WsMessageEvent};
use crate::channels::media::{attachment_from_bytes, mime_for_name, sniff_mime};
use crate::types::MessageAttachment;

/// What a WS callback needs to pull a message's images and files off the API.
/// Absent when the app could not be credentialed, in which case a media
/// message still reaches the agent — as its text label, never silently.
#[derive(Clone)]
pub(crate) struct MediaFetcher {
    pub(crate) http: reqwest::Client,
    pub(crate) base_url: String,
    pub(crate) app_id: String,
    pub(crate) app_secret: String,
    pub(crate) tokens: Arc<Mutex<HashMap<String, CachedToken>>>,
}

// ===== WS event types & processing =====

/// Process a raw WS event payload (JSON bytes from data frame) into an IncomingMessage
/// and dispatch to registered handlers. Runs synchronously inside the WS callback.
pub(crate) fn process_ws_event(
    payload: &[u8],
    app_id: &str,
    bot_open_id: &str,
    handlers: &Arc<RwLock<Vec<MessageCallback>>>,
    dedup: &Arc<Mutex<DedupState>>,
    sender_cache: &Arc<Mutex<HashMap<String, (String, i64)>>>,
    media: Option<&MediaFetcher>,
) {
    let envelope: WsEventEnvelope = match serde_json::from_slice(payload) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("[FeishuWS:{app_id}] Failed to parse event: {e}");
            return;
        }
    };

    let event_type = envelope.header.event_type.as_deref().unwrap_or("");
    if event_type != "im.message.receive_v1" {
        return;
    }

    let event = match envelope.event {
        Some(ref e) => e,
        None => return,
    };
    let msg_event: WsMessageEvent = match serde_json::from_value(event.clone()) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("[FeishuWS:{app_id}] Failed to parse message event: {e}");
            return;
        }
    };

    let message = match msg_event.message {
        Some(ref m) => m,
        None => return,
    };
    let sender = match msg_event.sender {
        Some(ref s) => s,
        None => return,
    };

    // Dedup
    {
        let mut dedup_guard = match dedup.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if !dedup_guard.try_record(&message.message_id, app_id) {
            return;
        }
    }

    let chat_jid = make_jid(&message.chat_type, &message.chat_id);
    let sender_open_id = sender
        .sender_id
        .as_ref()
        .and_then(|sid| sid.open_id.as_deref())
        .unwrap_or("");
    let sender_jid = if sender_open_id.is_empty() {
        String::new()
    } else {
        format!("feishu:user:{sender_open_id}")
    };

    // Sender name (best-effort from cache, sync-only)
    let sender_name = sender_cache
        .try_lock()
        .ok()
        .and_then(|cache| cache.get(sender_open_id).map(|(n, _)| n.clone()))
        .unwrap_or_else(|| {
            if sender_open_id.len() >= 8 {
                format!("{}...", &sender_open_id[..8])
            } else {
                sender_open_id.to_string()
            }
        });

    let content = parse_text_content(&message.content, &message.message_type);
    let is_mentioned = check_bot_mention(message.mentions.as_deref(), bot_open_id);
    let clean_content =
        remove_bot_mention_placeholders(&content, message.mentions.as_deref(), bot_open_id);

    let chat_type = match message.chat_type.as_str() {
        "p2p" | "private" => ChatType::Private,
        _ => ChatType::Group,
    };

    let incoming = IncomingMessage {
        id: format!("feishu:{app_id}:{}", message.message_id),
        chat_jid,
        sender_name,
        sender_jid,
        content: clean_content,
        timestamp: chrono::Utc::now().to_rfc3339(),
        is_from_me: false,
        chat_type,
        mentions_bot_username: Some(is_mentioned),
        bot_token: Some(app_id.to_string()),
        native_msg_id: Some(message.message_id.clone()),
        attachments: Vec::new(),
    };

    let refs = parse_media_refs(&message.content, &message.message_type);
    match (refs.is_empty(), media) {
        (true, _) | (false, None) => {
            if !refs.is_empty() {
                tracing::warn!(
                    "[FeishuWS:{app_id}] message {} carries {} resource(s) but the app has no \
                     credentials to fetch them; delivering the text only",
                    message.message_id,
                    refs.len()
                );
            }
            dispatch(handlers, incoming);
        }
        (false, Some(fetcher)) => {
            // Downloading is async and this callback is not, so the message is
            // dispatched from a task — after its blobs, so the agent sees one
            // turn carrying both.
            let fetcher = fetcher.clone();
            let handlers = Arc::clone(handlers);
            let message_id = message.message_id.clone();
            let app_id = app_id.to_string();
            tokio::spawn(async move {
                let attachments = fetch_media(&fetcher, &message_id, &refs).await;
                let mut incoming = incoming;
                // A caption-less image would otherwise reach the agent as an
                // empty turn, which reads as the user saying nothing.
                if incoming.content.trim().is_empty() && !attachments.is_empty() {
                    incoming.content = "[Ảnh đính kèm]".to_string();
                }
                if attachments.is_empty() {
                    tracing::warn!(
                        "[FeishuWS:{app_id}] no resource of message {message_id} could be fetched"
                    );
                }
                incoming.attachments = attachments;
                dispatch(&handlers, incoming);
            });
        }
    }
}

fn dispatch(handlers: &Arc<RwLock<Vec<MessageCallback>>>, incoming: IncomingMessage) {
    let Ok(guard) = handlers.read() else {
        return;
    };
    for handler in guard.iter() {
        handler(incoming.clone());
    }
}

/// Fetch every resource a message names. A failed one is logged and skipped
/// so the rest of the message still reaches the agent.
async fn fetch_media(
    fetcher: &MediaFetcher,
    message_id: &str,
    refs: &[super::helpers::MediaRef],
) -> Vec<MessageAttachment> {
    let token = match get_or_refresh_token(
        &fetcher.http,
        &fetcher.base_url,
        &fetcher.app_id,
        &fetcher.app_secret,
        &fetcher.tokens,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                "[FeishuWS:{}] token for media download failed: {e:#}",
                fetcher.app_id
            );
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for r in refs {
        match download_message_resource(
            &fetcher.http,
            &fetcher.base_url,
            &token,
            message_id,
            &r.key,
            r.kind,
        )
        .await
        {
            Ok(bytes) => {
                // The API sends no usable content type, so the bytes decide;
                // the file name is the fallback and `image` the last resort.
                let mime = sniff_mime(&bytes)
                    .map(str::to_string)
                    .or_else(|| r.name.as_deref().map(|n| mime_for_name(n).to_string()))
                    .unwrap_or_else(|| {
                        if r.kind == "image" {
                            "image/jpeg".to_string()
                        } else {
                            "application/octet-stream".to_string()
                        }
                    });
                tracing::info!(
                    "[FeishuWS:{}] downloaded {} bytes of {mime} media",
                    fetcher.app_id,
                    bytes.len()
                );
                out.push(attachment_from_bytes(&bytes, &mime, r.name.clone()));
            }
            Err(e) => tracing::warn!(
                "[FeishuWS:{}] resource {} of message {message_id} skipped: {e:#}",
                fetcher.app_id,
                r.key
            ),
        }
    }
    out
}

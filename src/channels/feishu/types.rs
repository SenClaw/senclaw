use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::helpers::now_ms;
use super::{DEDUP_CLEANUP_INTERVAL_MS, DEDUP_MAX_SIZE, DEDUP_TTL_MS};

// ===== FeishuDomain =====

#[derive(Debug, Clone, PartialEq)]
pub enum FeishuDomain {
    Feishu,
    Lark,
    Custom(String),
}

impl FeishuDomain {
    pub fn base_url(&self) -> &str {
        match self {
            FeishuDomain::Feishu => "https://open.feishu.cn",
            FeishuDomain::Lark => "https://open.larksuite.com",
            FeishuDomain::Custom(ref s) => s.as_str(),
        }
    }
}

// ===== FeishuBotInfo =====

#[derive(Debug, Clone)]
pub(crate) struct FeishuBotInfo {
    pub(crate) open_id: String,
    pub(crate) name: String,
}

// ===== Token types =====

#[derive(Debug, Clone)]
pub(crate) struct CachedToken {
    pub(crate) token: String,
    pub(crate) expires_at: i64, // unix timestamp
}

#[derive(Debug, Deserialize)]
pub(crate) struct TenantTokenResponse {
    pub(crate) code: i32,
    pub(crate) msg: Option<String>,
    pub(crate) tenant_access_token: Option<String>,
    pub(crate) expire: Option<i64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TenantTokenRequest<'a> {
    pub(crate) app_id: &'a str,
    pub(crate) app_secret: &'a str,
}

// ===== DedupState =====

pub(crate) struct DedupState {
    pub(crate) ids: HashMap<String, i64>, // key → added_at_ms
    pub(crate) last_cleanup_ms: i64,
}

impl DedupState {
    pub(crate) fn new() -> Self {
        Self {
            ids: HashMap::new(),
            last_cleanup_ms: now_ms(),
        }
    }

    /// Returns true if the message is new (not seen before).
    pub(crate) fn try_record(&mut self, message_id: &str, app_id: &str) -> bool {
        let now = now_ms();
        let key = format!("{app_id}:{message_id}");

        if now - self.last_cleanup_ms > DEDUP_CLEANUP_INTERVAL_MS {
            self.ids.retain(|_, ts| now - *ts <= DEDUP_TTL_MS);
            self.last_cleanup_ms = now;
        }

        if self.ids.contains_key(&key) {
            return false;
        }

        if self.ids.len() >= DEDUP_MAX_SIZE {
            if let Some(oldest_key) = self.ids.keys().next().cloned() {
                self.ids.remove(&oldest_key);
            }
        }

        self.ids.insert(key, now);
        true
    }
}

// ===== API types =====

#[derive(Debug, Deserialize)]
pub(crate) struct BotInfoResponse {
    pub(crate) code: i32,
    pub(crate) msg: Option<String>,
    pub(crate) bot: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SendMessageBody {
    pub(crate) receive_id: String,
    pub(crate) msg_type: String,
    pub(crate) content: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ApiResponse {
    pub(crate) code: i32,
    pub(crate) msg: Option<String>,
    pub(crate) data: Option<serde_json::Value>,
}

// ===== AppEntry =====

#[derive(Debug, Clone)]
pub(crate) struct AppEntry {
    pub(crate) base_url: String,
    pub(crate) app_id: String,
    pub(crate) app_secret: String,
}

// ===== WS event types =====

#[derive(Debug, Deserialize)]
pub(crate) struct WsEventEnvelope {
    pub(crate) header: WsEventHeader,
    pub(crate) event: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsEventHeader {
    #[serde(default)]
    #[serde(alias = "event_type")]
    #[serde(alias = "eventType")]
    pub(crate) event_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsMessageEvent {
    pub(crate) sender: Option<WsSender>,
    pub(crate) message: Option<WsMessage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsSender {
    pub(crate) sender_id: Option<WsSenderId>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsSenderId {
    pub(crate) open_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsMessage {
    pub(crate) message_id: String,
    pub(crate) chat_id: String,
    pub(crate) chat_type: String,
    pub(crate) message_type: String,
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) mentions: Option<Vec<serde_json::Value>>,
}

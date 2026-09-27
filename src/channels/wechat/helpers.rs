//! Constants, helpers, JID utilities, markdown-to-plain-text conversion,
//! and pending menu queue for the WeChat channel adapter.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::types::InlineButton;

use super::types::{WeixinAccountData, WeixinMessageItem};

// ===== Constants =====

pub(crate) const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
pub(crate) const DEFAULT_LONG_POLL_TIMEOUT_MS: u64 = 35_000;
pub(crate) const DEFAULT_API_TIMEOUT_MS: u64 = 15_000;
pub(crate) const QR_LONG_POLL_TIMEOUT_MS: u64 = 35_000;
pub(crate) const MAX_QR_REFRESH: u32 = 3;
pub(crate) const MAX_CONSECUTIVE_FAILURES: u32 = 3;
pub(crate) const BACKOFF_DELAY_SECS: u64 = 30;
pub(crate) const RETRY_DELAY_SECS: u64 = 2;
pub(crate) const SESSION_EXPIRED_ERRCODE: i32 = -14;
pub(crate) const MENU_TTL_SECS: u64 = 5 * 60;
pub(crate) const WECHAT_MAX_LEN: usize = 2000;

pub(crate) const MSG_TYPE_USER: u32 = 1;
pub(crate) const MSG_TYPE_BOT: u32 = 2;
pub(crate) const ITEM_TYPE_TEXT: u32 = 1;
pub(crate) const ITEM_TYPE_VOICE: u32 = 3;
pub(crate) const ITEM_TYPE_IMAGE: u32 = 2;
pub(crate) const ITEM_TYPE_FILE: u32 = 4;
pub(crate) const ITEM_TYPE_VIDEO: u32 = 5;
pub(crate) const MSG_STATE_FINISH: u32 = 2;

// ===== Helpers =====

/// The three headers every iLink Bot API call carries.
pub(crate) fn with_api_headers(
    req: reqwest::RequestBuilder,
    token: &str,
) -> reqwest::RequestBuilder {
    req.header("Content-Type", "application/json")
        .header("AuthorizationType", "ilink_bot_token")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-WECHAT-UIN", random_wechat_uin())
}

pub(crate) fn random_hex(n: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..n).map(|_| format!("{:02x}", rng.gen::<u8>())).collect()
}

pub(crate) fn random_wechat_uin() -> String {
    use rand::Rng;
    let u: u32 = rand::thread_rng().gen();
    u.to_string()
}

fn wechat_state_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".senclaw")
        .join("wechat")
        .join("accounts")
}

fn account_path(account_id: &str) -> PathBuf {
    wechat_state_dir().join(format!("{account_id}.json"))
}

fn sync_buf_path(account_id: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".senclaw")
        .join("wechat")
        .join(format!("sync-buf-{account_id}.bin"))
}

fn context_tokens_path(account_id: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".senclaw")
        .join("wechat")
        .join(format!("context-tokens-{account_id}.json"))
}

pub(crate) fn load_account(account_id: &str) -> Option<WeixinAccountData> {
    let p = account_path(account_id);
    if !p.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&p).ok()?;
    serde_json::from_str(&raw).ok()
}

pub(crate) fn save_account(account_id: &str, data: &WeixinAccountData) {
    let p = account_path(account_id);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let raw = serde_json::to_string_pretty(data).unwrap_or_default();
    let _ = std::fs::write(&p, raw);
}

pub(crate) fn load_sync_buf(account_id: &str) -> String {
    let p = sync_buf_path(account_id);
    std::fs::read_to_string(&p)
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub(crate) fn save_sync_buf(account_id: &str, buf: &str) {
    let p = sync_buf_path(account_id);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, buf);
}

pub(crate) fn load_context_tokens(account_id: &str) -> HashMap<String, String> {
    let p = context_tokens_path(account_id);
    if !p.exists() {
        return HashMap::new();
    }
    let raw = std::fs::read_to_string(&p).unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or_default()
}

pub(crate) fn save_context_tokens(account_id: &str, tokens: &HashMap<String, String>) {
    let p = context_tokens_path(account_id);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let raw = serde_json::to_string(tokens).unwrap_or_default();
    let _ = std::fs::write(&p, raw);
}

// ===== JID utilities =====

pub(crate) fn user_id_to_jid(user_id: &str) -> String {
    format!("wx:user:{user_id}")
}

pub(crate) fn jid_to_user_id(jid: &str) -> Option<&str> {
    jid.strip_prefix("wx:user:")
}

// ===== Markdown to plain text =====

pub(crate) fn markdown_to_plain(text: &str) -> String {
    let mut r = text.to_string();
    // Code blocks: keep content, remove fences
    r = regex::Regex::new(r"```[^\n]*\n?([\s\S]*?)```")
        .unwrap()
        .replace_all(&r, |caps: &regex::Captures| caps[1].trim().to_string())
        .to_string();
    // Images: remove entirely
    r = regex::Regex::new(r"!\[[^\]]*\]\([^)]*\)")
        .unwrap()
        .replace_all(&r, "")
        .to_string();
    // Links: keep display text
    r = regex::Regex::new(r"\[([^\]]+)\]\([^)]*\)")
        .unwrap()
        .replace_all(&r, "$1")
        .to_string();
    // Table separator rows: remove
    r = regex::Regex::new(r"^\|[\s:|-]+\|$")
        .unwrap()
        .replace_all(&r, "")
        .to_string();
    // Bold/italic markers
    r = regex::Regex::new(r"\*\*([^*]+)\*\*")
        .unwrap()
        .replace_all(&r, "$1")
        .to_string();
    r = regex::Regex::new(r"\*([^*]+)\*")
        .unwrap()
        .replace_all(&r, "$1")
        .to_string();
    r = regex::Regex::new(r"__([^_]+)__")
        .unwrap()
        .replace_all(&r, "$1")
        .to_string();
    r = regex::Regex::new(r"_([^_]+)_")
        .unwrap()
        .replace_all(&r, "$1")
        .to_string();
    // Headings: remove # prefix
    r = regex::Regex::new(r"^#{1,6}\s+")
        .unwrap()
        .replace_all(&r, "")
        .to_string();
    // Inline code backticks
    r = regex::Regex::new(r"`([^`]+)`")
        .unwrap()
        .replace_all(&r, "$1")
        .to_string();
    r.trim().to_string()
}

pub(crate) fn split_text(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut remaining = text;
    while remaining.len() > max_len {
        let chunk = &remaining[..max_len];
        let at = chunk
            .rfind('\n')
            .filter(|&pos| pos > max_len / 2)
            .unwrap_or(max_len);
        parts.push(remaining[..at].to_string());
        remaining = remaining[at..].trim_start_matches('\n');
    }
    if !remaining.is_empty() {
        parts.push(remaining.to_string());
    }
    parts
}

pub(crate) fn extract_text(items: Option<&[WeixinMessageItem]>) -> String {
    let items = match items {
        Some(v) if !v.is_empty() => v,
        _ => return String::new(),
    };
    for item in items {
        if item.item_type == Some(ITEM_TYPE_TEXT) {
            if let Some(ref ti) = item.text_item {
                if let Some(ref text) = ti.text {
                    return text.clone();
                }
            }
        }
        if item.item_type == Some(ITEM_TYPE_VOICE) {
            if let Some(ref vi) = item.voice_item {
                if let Some(ref text) = vi.text {
                    return format!("[Voice] {text}");
                }
            }
        }
    }
    let type_label = match items[0].item_type {
        Some(ITEM_TYPE_IMAGE) => "[Image]",
        Some(ITEM_TYPE_VOICE) => "[Voice]",
        Some(ITEM_TYPE_FILE) => "[File]",
        Some(ITEM_TYPE_VIDEO) => "[Video]",
        _ => "[Message]",
    };
    type_label.to_string()
}

// ===== Pending menu queue =====

pub(crate) struct PendingMenuEntry {
    pub(crate) options: Vec<InlineButton>,
    pub(crate) created_at: Instant,
}

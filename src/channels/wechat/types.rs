//! JSON types for WeChat iLink Bot API.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WeixinAccountData {
    pub(crate) token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) base_url: Option<String>,
    #[serde(rename = "userId", skip_serializing_if = "Option::is_none")]
    pub(crate) user_id: Option<String>,
    #[serde(rename = "savedAt")]
    pub(crate) saved_at: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct SendMessageRequest {
    pub(crate) msg: SendMessageMsg,
}

#[derive(Debug, Serialize)]
pub(crate) struct SendMessageMsg {
    pub(crate) from_user_id: String,
    pub(crate) to_user_id: String,
    pub(crate) client_id: String,
    pub(crate) message_type: u32,
    pub(crate) message_state: u32,
    pub(crate) item_list: Vec<MessageItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context_token: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct MessageItem {
    #[serde(rename = "type")]
    pub(crate) item_type: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text_item: Option<TextItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) image_item: Option<OutImageItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) file_item: Option<OutFileItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) video_item: Option<OutVideoItem>,
}

#[derive(Debug, Serialize)]
pub(crate) struct TextItem {
    pub(crate) text: String,
}

/// CDN reference the bot sends back after an upload. `aes_key` is base64 of
/// the key's 32-char hex spelling (what the official sender emits);
/// `encrypt_type` is always 1 (AES-128-ECB).
#[derive(Debug, Serialize)]
pub(crate) struct OutCdnMedia {
    pub(crate) encrypt_query_param: String,
    pub(crate) aes_key: String,
    pub(crate) encrypt_type: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct OutImageItem {
    pub(crate) media: OutCdnMedia,
    /// Ciphertext size in bytes.
    pub(crate) mid_size: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct OutFileItem {
    pub(crate) media: OutCdnMedia,
    pub(crate) file_name: String,
    /// Plaintext size, as a decimal string (the API's spelling).
    pub(crate) len: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct OutVideoItem {
    pub(crate) media: OutCdnMedia,
    /// Ciphertext size in bytes.
    pub(crate) video_size: u64,
}

/// `getuploadurl` request. `aeskey` is the 16-byte key as hex; `filesize` is
/// the ciphertext size (PKCS#7 padded), `rawsize` the plaintext size.
#[derive(Debug, Serialize)]
pub(crate) struct GetUploadUrlRequest {
    pub(crate) filekey: String,
    pub(crate) media_type: u32,
    pub(crate) to_user_id: String,
    pub(crate) rawsize: u64,
    pub(crate) rawfilemd5: String,
    pub(crate) filesize: u64,
    pub(crate) no_need_thumb: bool,
    pub(crate) aeskey: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GetUploadUrlResponse {
    pub(crate) ret: Option<i32>,
    pub(crate) errcode: Option<i32>,
    pub(crate) errmsg: Option<String>,
    pub(crate) upload_param: Option<String>,
    pub(crate) upload_full_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GetUpdatesResponse {
    pub(crate) ret: Option<i32>,
    pub(crate) errcode: Option<i32>,
    pub(crate) errmsg: Option<String>,
    pub(crate) msgs: Option<Vec<WeixinMessage>>,
    pub(crate) get_updates_buf: Option<String>,
    pub(crate) longpolling_timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WeixinMessage {
    #[serde(rename = "message_id")]
    pub(crate) message_id: Option<u64>,
    pub(crate) from_user_id: Option<String>,
    pub(crate) create_time_ms: Option<i64>,
    pub(crate) message_type: Option<u32>,
    pub(crate) item_list: Option<Vec<WeixinMessageItem>>,
    pub(crate) context_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WeixinMessageItem {
    #[serde(rename = "type")]
    pub(crate) item_type: Option<u32>,
    pub(crate) text_item: Option<WeixinTextItem>,
    /// Voice arrives SILK-encoded with a server transcript in `text`; only the
    /// transcript is used.
    pub(crate) voice_item: Option<WeixinTextItem>,
    pub(crate) image_item: Option<WeixinImageItem>,
    pub(crate) file_item: Option<WeixinFileItem>,
    pub(crate) video_item: Option<WeixinVideoItem>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WeixinTextItem {
    pub(crate) text: Option<String>,
}

/// Where an inbound blob lives on the CDN and how to unlock it. `aes_key` is
/// base64 of either the 16 raw key bytes or their 32-char hex spelling.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct WeixinCdnMedia {
    pub(crate) encrypt_query_param: Option<String>,
    pub(crate) aes_key: Option<String>,
    pub(crate) encrypt_type: Option<u32>,
    pub(crate) full_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WeixinImageItem {
    pub(crate) media: Option<WeixinCdnMedia>,
    /// Key as 32 hex chars; wins over `media.aes_key` when both are present.
    pub(crate) aeskey: Option<String>,
    pub(crate) mid_size: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WeixinFileItem {
    pub(crate) media: Option<WeixinCdnMedia>,
    pub(crate) file_name: Option<String>,
    /// Plaintext size as a decimal string.
    pub(crate) len: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WeixinVideoItem {
    pub(crate) media: Option<WeixinCdnMedia>,
    pub(crate) video_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct QrCodeResponse {
    pub(crate) qrcode: String,
    pub(crate) qrcode_img_content: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct QrStatusResponse {
    pub(crate) status: String,
    pub(crate) bot_token: Option<String>,
    pub(crate) ilink_bot_id: Option<String>,
    pub(crate) baseurl: Option<String>,
    pub(crate) ilink_user_id: Option<String>,
}

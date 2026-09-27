//! Media on the iLink Bot API — inbound download + decrypt, outbound
//! encrypt + upload.
//!
//! Every blob on the WeChat CDN is AES-128-ECB / PKCS#7 encrypted with a
//! per-message key. Inbound, `media.encrypt_query_param` (or `full_url`) names
//! the blob and `media.aes_key` (base64 of 16 raw bytes *or* of their hex
//! spelling) unlocks it; an image's `aeskey` (hex) wins when present. A wrong
//! key does not fail — it yields garbage — so an image is only attached when
//! the plaintext starts with a known signature. Outbound: random key, encrypt,
//! `getuploadurl`, POST the ciphertext to the CDN, and send the item with the
//! CDN's `x-encrypted-param` echo. Wire details follow
//! Tencent/openclaw-weixin `src/cdn` (MIT).

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};

use crate::channels::media::{attachment_from_bytes, mime_for_name, sniff_mime};
use crate::types::MessageAttachment;

use super::helpers::{
    with_api_headers, DEFAULT_API_TIMEOUT_MS, ITEM_TYPE_FILE, ITEM_TYPE_IMAGE, ITEM_TYPE_VIDEO,
};
use super::types::{
    GetUploadUrlRequest, GetUploadUrlResponse, WeixinCdnMedia, WeixinMessageItem,
};

/// Default CDN when the server sends no `full_url`.
pub(crate) const CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
/// Largest image we hand to a vision model or OCR (same cap as Telegram).
pub(crate) const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Largest file/video we attach; documents past this are dropped with a log line.
pub(crate) const MAX_FILE_BYTES: usize = crate::agent::documents::MAX_DOC_BYTES;

/// `media_type` values for `getuploadurl`.
pub(crate) const UPLOAD_MEDIA_IMAGE: u32 = 1;
pub(crate) const UPLOAD_MEDIA_VIDEO: u32 = 2;
pub(crate) const UPLOAD_MEDIA_FILE: u32 = 3;

// ===== AES-128-ECB =====

/// Decode a `media.aes_key`: base64 of 16 raw bytes, or base64 of the key's
/// 32-char hex spelling (what the official sender emits).
pub(crate) fn parse_aes_key(b64: &str) -> Result<[u8; 16]> {
    let raw = STANDARD
        .decode(b64.trim())
        .context("aes_key is not base64")?;
    key_from_bytes(&raw)
}

/// Decode an image's `aeskey` (32 hex chars).
pub(crate) fn parse_hex_key(hex_key: &str) -> Result<[u8; 16]> {
    let raw = hex::decode(hex_key.trim()).context("aeskey is not hex")?;
    key_from_bytes(&raw)
}

fn key_from_bytes(raw: &[u8]) -> Result<[u8; 16]> {
    if raw.len() == 16 {
        return Ok(raw.try_into().expect("16 bytes"));
    }
    if raw.len() == 32 && raw.iter().all(u8::is_ascii_hexdigit) {
        let decoded = hex::decode(raw).context("hex key")?;
        return Ok(decoded.try_into().expect("16 bytes"));
    }
    Err(anyhow!(
        "aes key must be 16 bytes or 32 hex chars, got {} bytes",
        raw.len()
    ))
}

/// Ciphertext length for a PKCS#7-padded plaintext of `n` bytes.
#[cfg(test)]
pub(crate) fn padded_len(n: usize) -> usize {
    (n / 16 + 1) * 16
}

pub(crate) fn aes128_ecb_encrypt(plain: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let pad = 16 - (plain.len() % 16);
    let mut buf = Vec::with_capacity(plain.len() + pad);
    buf.extend_from_slice(plain);
    buf.extend(std::iter::repeat(pad as u8).take(pad));
    for chunk in buf.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }
    buf
}

pub(crate) fn aes128_ecb_decrypt(data: &[u8], key: &[u8; 16]) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        return Err(anyhow!(
            "ciphertext length {} is not a multiple of 16",
            data.len()
        ));
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut buf = data.to_vec();
    for chunk in buf.chunks_exact_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
    }
    let pad = *buf.last().expect("non-empty") as usize;
    if pad == 0 || pad > 16 || buf.len() < pad || !buf[buf.len() - pad..].iter().all(|&b| b as usize == pad) {
        return Err(anyhow!("bad PKCS#7 padding — wrong key?"));
    }
    buf.truncate(buf.len() - pad);
    Ok(buf)
}

// ===== URLs =====

/// Download URL for an inbound blob: the server's `full_url` when present,
/// else the default CDN's `/download` with the encrypted query param.
pub(crate) fn download_url(media: &WeixinCdnMedia) -> Option<String> {
    if let Some(url) = media.full_url.as_deref().filter(|u| !u.is_empty()) {
        return Some(url.to_string());
    }
    let param = media.encrypt_query_param.as_deref().filter(|p| !p.is_empty())?;
    Some(format!(
        "{CDN_BASE_URL}/download?encrypted_query_param={}",
        urlencoding::encode(param)
    ))
}

fn upload_url(full_url: Option<&str>, upload_param: &str, filekey: &str) -> String {
    if let Some(u) = full_url.filter(|u| !u.is_empty()) {
        return u.to_string();
    }
    format!(
        "{CDN_BASE_URL}/upload?encrypted_query_param={}&filekey={}",
        urlencoding::encode(upload_param),
        urlencoding::encode(filekey)
    )
}

// ===== Inbound =====

/// Which inbound items carry a downloadable blob, with the key and cap for each.
struct InboundRef {
    media: WeixinCdnMedia,
    /// `None` when the item says the blob is not encrypted (`encrypt_type` 0).
    key: Option<[u8; 16]>,
    name: Option<String>,
    /// MIME the item type implies; the sniffed one wins when it knows better.
    fallback_mime: &'static str,
    max_bytes: usize,
    /// Size the item claims, used to refuse an oversized blob before spending
    /// the download. Images/videos declare ciphertext bytes, files plaintext.
    declared_size: Option<u64>,
    /// Images must decrypt to a recognised picture; anything else is a wrong
    /// key and must not reach the model as "an image".
    require_signature: bool,
}

/// `encrypt_type` 0 means the CDN holds the blob in the clear.
fn is_plaintext(media: &WeixinCdnMedia) -> bool {
    media.encrypt_type == Some(0)
}

fn inbound_ref(item: &WeixinMessageItem) -> Option<InboundRef> {
    match item.item_type {
        Some(ITEM_TYPE_IMAGE) => {
            let img = item.image_item.as_ref()?;
            let media = img.media.clone()?;
            let key = if is_plaintext(&media) {
                None
            } else {
                match img.aeskey.as_deref().filter(|k| !k.is_empty()) {
                    Some(h) => Some(parse_hex_key(h).ok()?),
                    None => Some(parse_aes_key(media.aes_key.as_deref()?).ok()?),
                }
            };
            Some(InboundRef {
                media,
                key,
                name: None,
                fallback_mime: "image/jpeg",
                max_bytes: MAX_IMAGE_BYTES,
                declared_size: img.mid_size,
                require_signature: true,
            })
        }
        Some(ITEM_TYPE_FILE) => {
            let f = item.file_item.as_ref()?;
            let media = f.media.clone()?;
            let key = if is_plaintext(&media) {
                None
            } else {
                Some(parse_aes_key(media.aes_key.as_deref()?).ok()?)
            };
            let name = f.file_name.clone().filter(|n| !n.is_empty());
            let fallback_mime = name
                .as_deref()
                .map(mime_for_name)
                .unwrap_or("application/octet-stream");
            Some(InboundRef {
                media,
                key,
                name,
                fallback_mime,
                max_bytes: MAX_FILE_BYTES,
                declared_size: f.len.as_deref().and_then(|l| l.parse().ok()),
                require_signature: false,
            })
        }
        Some(ITEM_TYPE_VIDEO) => {
            let v = item.video_item.as_ref()?;
            let media = v.media.clone()?;
            let key = if is_plaintext(&media) {
                None
            } else {
                Some(parse_aes_key(media.aes_key.as_deref()?).ok()?)
            };
            Some(InboundRef {
                media,
                key,
                name: Some("video.mp4".to_string()),
                fallback_mime: "video/mp4",
                max_bytes: MAX_FILE_BYTES,
                declared_size: v.video_size,
                require_signature: false,
            })
        }
        _ => None,
    }
}

/// Download and decrypt every image/file/video item of a message. A failed
/// item is logged and skipped so the text still reaches the agent.
pub(crate) async fn download_attachments(
    http: &reqwest::Client,
    items: Option<&[WeixinMessageItem]>,
) -> Vec<MessageAttachment> {
    let mut out = Vec::new();
    let Some(items) = items else {
        return out;
    };
    for item in items {
        let Some(r) = inbound_ref(item) else {
            continue;
        };
        match fetch_inbound(http, &r).await {
            Ok(a) => out.push(a),
            Err(e) => tracing::warn!(
                "[WeChatChannel] media item type={:?} skipped: {e:#}",
                item.item_type
            ),
        }
    }
    out
}

async fn fetch_inbound(http: &reqwest::Client, r: &InboundRef) -> Result<MessageAttachment> {
    if let Some(declared) = r.declared_size {
        if declared as usize > r.max_bytes {
            return Err(anyhow!(
                "item declares {declared} bytes, over the {} byte cap",
                r.max_bytes
            ));
        }
    }
    let url = download_url(&r.media).ok_or_else(|| anyhow!("item has no CDN reference"))?;
    let resp = http
        .get(&url)
        .send()
        .await
        .context("CDN download")?;
    if !resp.status().is_success() {
        return Err(anyhow!("CDN download HTTP {}", resp.status()));
    }
    if let Some(len) = resp.content_length() {
        if len as usize > r.max_bytes + 16 {
            return Err(anyhow!("{len} bytes is over the {} byte cap", r.max_bytes));
        }
    }
    let body = resp.bytes().await.context("CDN body")?;
    if body.len() > r.max_bytes + 16 {
        return Err(anyhow!("{} bytes is over the {} byte cap", body.len(), r.max_bytes));
    }
    let plain = match r.key {
        Some(key) => aes128_ecb_decrypt(&body, &key)?,
        None => body.to_vec(),
    };
    let sniffed = sniff_mime(&plain);
    if r.require_signature && sniffed.map_or(true, |m| !m.starts_with("image/")) {
        return Err(anyhow!(
            "decrypted image has no picture signature (wrong key or unsupported format)"
        ));
    }
    let mime = sniffed.unwrap_or(r.fallback_mime);
    tracing::info!(
        "[WeChatChannel] downloaded {} bytes of {mime} media",
        plain.len()
    );
    Ok(attachment_from_bytes(&plain, mime, r.name.clone()))
}

// ===== Outbound =====

/// What `sendmessage` needs after a successful upload.
pub(crate) struct UploadedMedia {
    pub(crate) encrypt_query_param: String,
    /// base64 of the key's hex spelling — the form the official sender uses.
    pub(crate) aes_key_b64: String,
    pub(crate) raw_size: u64,
    pub(crate) cipher_size: u64,
}

/// Encrypt `bytes`, ask the API for an upload slot, POST the ciphertext to
/// the CDN and return what the message item must carry.
pub(crate) async fn upload_to_cdn(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    to_user_id: &str,
    media_type: u32,
    bytes: &[u8],
) -> Result<UploadedMedia> {
    use rand::RngCore;
    let mut key = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut key);
    let mut filekey_raw = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut filekey_raw);
    let filekey = hex::encode(filekey_raw);
    let key_hex = hex::encode(key);

    let ciphertext = aes128_ecb_encrypt(bytes, &key);
    let req = GetUploadUrlRequest {
        filekey: filekey.clone(),
        media_type,
        to_user_id: to_user_id.to_string(),
        rawsize: bytes.len() as u64,
        rawfilemd5: format!("{:x}", md5::compute(bytes)),
        filesize: ciphertext.len() as u64,
        no_need_thumb: true,
        aeskey: key_hex.clone(),
    };
    let body = serde_json::to_string(&req)?;
    let resp = with_api_headers(http.post(format!("{base_url}/ilink/bot/getuploadurl")), token)
        .header("Content-Length", body.len())
        .body(body)
        .timeout(std::time::Duration::from_millis(DEFAULT_API_TIMEOUT_MS))
        .send()
        .await
        .context("getuploadurl request")?;
    let status = resp.status();
    let text = resp.text().await.context("getuploadurl body")?;
    if !status.is_success() {
        return Err(anyhow!("getuploadurl HTTP {status}: {text}"));
    }
    let parsed: GetUploadUrlResponse =
        serde_json::from_str(&text).with_context(|| format!("parse getuploadurl: {text}"))?;
    if parsed.ret.is_some_and(|r| r != 0) || parsed.errcode.is_some_and(|e| e != 0) {
        return Err(anyhow!(
            "getuploadurl ret={:?} errcode={:?} errmsg={:?}",
            parsed.ret,
            parsed.errcode,
            parsed.errmsg
        ));
    }
    let upload_param = parsed.upload_param.unwrap_or_default();
    if upload_param.is_empty() && parsed.upload_full_url.as_deref().map_or(true, str::is_empty) {
        return Err(anyhow!("getuploadurl returned neither upload_param nor upload_full_url"));
    }
    let url = upload_url(parsed.upload_full_url.as_deref(), &upload_param, &filekey);

    // The CDN answers 4xx for a bad slot (no point retrying) and echoes the
    // download parameter in a header on success.
    let mut last_err = None;
    for attempt in 1..=3 {
        let resp = http
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .body(ciphertext.clone())
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let param = r
                    .headers()
                    .get("x-encrypted-param")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| anyhow!("CDN upload succeeded without x-encrypted-param"))?;
                return Ok(UploadedMedia {
                    encrypt_query_param: param,
                    aes_key_b64: STANDARD.encode(key_hex.as_bytes()),
                    raw_size: bytes.len() as u64,
                    cipher_size: ciphertext.len() as u64,
                });
            }
            Ok(r) if r.status().is_client_error() => {
                return Err(anyhow!("CDN upload HTTP {}", r.status()));
            }
            Ok(r) => last_err = Some(anyhow!("CDN upload HTTP {} (attempt {attempt})", r.status())),
            Err(e) => last_err = Some(anyhow!("CDN upload failed (attempt {attempt}): {e}")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("CDN upload failed")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{WeixinFileItem, WeixinImageItem};

    const KEY: [u8; 16] = *b"0123456789abcdef";

    #[test]
    fn ecb_roundtrip_and_padding() {
        for len in [0usize, 1, 15, 16, 17, 100] {
            let plain: Vec<u8> = (0..len as u8).collect();
            let ct = aes128_ecb_encrypt(&plain, &KEY);
            assert_eq!(ct.len(), padded_len(len));
            assert_eq!(aes128_ecb_decrypt(&ct, &KEY).unwrap(), plain);
        }
    }

    #[test]
    fn ecb_matches_known_vector() {
        // AES-128 ECB, FIPS-197 C.1: key 000102…0f, block 00112233…ff.
        let key: [u8; 16] = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap().try_into().unwrap();
        let block = hex::decode("00112233445566778899aabbccddeeff").unwrap();
        let ct = aes128_ecb_encrypt(&block, &key);
        assert_eq!(hex::encode(&ct[..16]), "69c4e0d86a7b0430d8cdb78070b4c55a");
        // One full block of plaintext gets a whole padding block appended.
        assert_eq!(ct.len(), 32);
    }

    #[test]
    fn wrong_key_is_rejected_by_padding_check() {
        let ct = aes128_ecb_encrypt(b"hello world", &KEY);
        let other = *b"fedcba9876543210";
        // Random garbage almost never ends in valid PKCS#7; a 1-in-256 false
        // pass is why images are additionally signature-checked.
        let r = aes128_ecb_decrypt(&ct, &other);
        assert!(r.is_err() || r.unwrap() != b"hello world");
    }

    #[test]
    fn parses_both_key_spellings() {
        let raw_b64 = STANDARD.encode(KEY);
        assert_eq!(parse_aes_key(&raw_b64).unwrap(), KEY);
        let hex_b64 = STANDARD.encode(hex::encode(KEY).as_bytes());
        assert_eq!(parse_aes_key(&hex_b64).unwrap(), KEY);
        assert_eq!(parse_hex_key(&hex::encode(KEY)).unwrap(), KEY);
        assert!(parse_aes_key(&STANDARD.encode(b"short")).is_err());
    }

    #[test]
    fn download_url_prefers_full_url() {
        let m = WeixinCdnMedia {
            encrypt_query_param: Some("a b&c".into()),
            full_url: None,
            ..Default::default()
        };
        assert_eq!(
            download_url(&m).unwrap(),
            format!("{CDN_BASE_URL}/download?encrypted_query_param=a%20b%26c")
        );
        let m2 = WeixinCdnMedia {
            full_url: Some("https://cdn.example/x".into()),
            ..m
        };
        assert_eq!(download_url(&m2).unwrap(), "https://cdn.example/x");
        assert!(download_url(&WeixinCdnMedia::default()).is_none());
    }

    #[test]
    fn upload_url_fallback_shape() {
        assert_eq!(
            upload_url(None, "p=1", "abc"),
            format!("{CDN_BASE_URL}/upload?encrypted_query_param=p%3D1&filekey=abc")
        );
        assert_eq!(upload_url(Some("https://u/x"), "p", "k"), "https://u/x");
    }

    #[test]
    fn inbound_ref_reads_image_and_file_items() {
        let img = WeixinMessageItem {
            item_type: Some(ITEM_TYPE_IMAGE),
            image_item: Some(WeixinImageItem {
                media: Some(WeixinCdnMedia {
                    encrypt_query_param: Some("q".into()),
                    aes_key: Some(STANDARD.encode(KEY)),
                    ..Default::default()
                }),
                aeskey: Some(hex::encode(*b"fedcba9876543210")),
                ..Default::default()
            }),
            ..Default::default()
        };
        let r = inbound_ref(&img).unwrap();
        assert_eq!(r.key.unwrap(), *b"fedcba9876543210", "hex aeskey wins over media.aes_key");
        assert!(r.require_signature);
        assert_eq!(r.max_bytes, MAX_IMAGE_BYTES);

        let file = WeixinMessageItem {
            item_type: Some(ITEM_TYPE_FILE),
            file_item: Some(WeixinFileItem {
                media: Some(WeixinCdnMedia {
                    encrypt_query_param: Some("q".into()),
                    aes_key: Some(STANDARD.encode(KEY)),
                    ..Default::default()
                }),
                file_name: Some("report.pdf".into()),
                len: Some("123".into()),
            }),
            ..Default::default()
        };
        let r = inbound_ref(&file).unwrap();
        assert_eq!(r.name.as_deref(), Some("report.pdf"));
        assert_eq!(r.fallback_mime, "application/pdf");
        assert_eq!(r.declared_size, Some(123));
        assert!(!r.require_signature);

        // `encrypt_type` 0 means the blob is stored in the clear.
        let plain_img = WeixinMessageItem {
            item_type: Some(ITEM_TYPE_IMAGE),
            image_item: Some(WeixinImageItem {
                media: Some(WeixinCdnMedia {
                    encrypt_query_param: Some("q".into()),
                    encrypt_type: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(inbound_ref(&plain_img).unwrap().key.is_none());

        // A file item without a key cannot be decrypted and is not a candidate.
        let keyless = WeixinMessageItem {
            item_type: Some(ITEM_TYPE_FILE),
            file_item: Some(WeixinFileItem {
                media: Some(WeixinCdnMedia {
                    encrypt_query_param: Some("q".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(inbound_ref(&keyless).is_none());
    }

    #[test]
    fn parses_getupdates_media_items() {
        let raw = r#"{"type":2,"image_item":{"media":{"encrypt_query_param":"abc","aes_key":"MDEyMzQ1Njc4OWFiY2RlZg==","encrypt_type":1},"mid_size":4096}}"#;
        let item: WeixinMessageItem = serde_json::from_str(raw).unwrap();
        assert_eq!(item.item_type, Some(ITEM_TYPE_IMAGE));
        let r = inbound_ref(&item).unwrap();
        assert_eq!(r.key.unwrap(), KEY);
        let raw = r#"{"type":4,"file_item":{"media":{"encrypt_query_param":"abc","aes_key":"MDEyMzQ1Njc4OWFiY2RlZg=="},"file_name":"a.txt","len":"5"}}"#;
        let item: WeixinMessageItem = serde_json::from_str(raw).unwrap();
        assert_eq!(inbound_ref(&item).unwrap().name.as_deref(), Some("a.txt"));
    }
}

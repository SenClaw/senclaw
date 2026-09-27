use std::time::{SystemTime, UNIX_EPOCH};

use super::FEISHU_MAX_LEN;

// ===== Time helpers =====

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ===== Content parsing =====

pub(crate) fn parse_text_content(content: &str, message_type: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return content.to_string(),
    };

    if message_type == "text" {
        return parsed["text"].as_str().unwrap_or("").to_string();
    }

    if message_type == "post" {
        let blocks = parsed["zh_cn"]["content"]
            .as_array()
            .or_else(|| parsed["en_us"]["content"].as_array())
            .or_else(|| parsed["content"].as_array());

        let mut lines: Vec<String> = Vec::new();
        if let Some(blocks) = blocks {
            for paragraph in blocks {
                let arr = match paragraph.as_array() {
                    Some(a) => a,
                    None => continue,
                };
                let line: String = arr
                    .iter()
                    .map(|node| match node["tag"].as_str().unwrap_or("") {
                        "text" => node["text"].as_str().unwrap_or("").to_string(),
                        "a" => node["text"]
                            .as_str()
                            .or_else(|| node["href"].as_str())
                            .unwrap_or("")
                            .to_string(),
                        "at" => String::new(),
                        "img" => "[Image]".to_string(),
                        _ => node["text"].as_str().unwrap_or("").to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    lines.push(trimmed);
                }
            }
        }

        let title = parsed["zh_cn"]["title"]
            .as_str()
            .or_else(|| parsed["en_us"]["title"].as_str())
            .unwrap_or("");
        let body = lines.join("\n");
        return if title.is_empty() {
            body
        } else {
            format!("{title}\n{body}")
        };
    }

    // A media message has no text of its own. Falling through to `content`
    // here would hand the agent the raw `{"image_key":…}` JSON as if the user
    // had typed it; the label is replaced by the caption-less marker once the
    // blob is actually attached.
    match message_type {
        "image" => return "[Image]".to_string(),
        "file" => {
            return match parsed["file_name"].as_str().filter(|n| !n.is_empty()) {
                Some(name) => format!("[File] {name}"),
                None => "[File]".to_string(),
            }
        }
        "audio" => return "[Audio]".to_string(),
        "media" => return "[Video]".to_string(),
        "sticker" => return "[Sticker]".to_string(),
        _ => {}
    }

    parsed["text"].as_str().unwrap_or(content).to_string()
}

/// One downloadable resource named by a message's `content`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MediaRef {
    /// `image_key` or `file_key`.
    pub(crate) key: String,
    /// The `type` query param: `"image"` or `"file"`.
    pub(crate) kind: &'static str,
    /// Original name, when the message carried one.
    pub(crate) name: Option<String>,
}

/// Resources named by a message, in the order they appear.
///
/// `media` (video) and `audio` both file their blob under `file_key`, and a
/// rich-text `post` can embed several images. A video's poster `image_key` is
/// skipped: it is a thumbnail of the same content, and attaching both makes
/// the model describe the picture twice.
pub(crate) fn parse_media_refs(content: &str, message_type: &str) -> Vec<MediaRef> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) else {
        return Vec::new();
    };
    let file_name = parsed["file_name"].as_str().map(str::to_string);
    match message_type {
        "image" => parsed["image_key"]
            .as_str()
            .filter(|k| !k.is_empty())
            .map(|k| {
                vec![MediaRef {
                    key: k.to_string(),
                    kind: "image",
                    name: None,
                }]
            })
            .unwrap_or_default(),
        "file" | "audio" | "media" => parsed["file_key"]
            .as_str()
            .filter(|k| !k.is_empty())
            .map(|k| {
                vec![MediaRef {
                    key: k.to_string(),
                    kind: "file",
                    name: file_name,
                }]
            })
            .unwrap_or_default(),
        "post" => {
            let blocks = parsed["zh_cn"]["content"]
                .as_array()
                .or_else(|| parsed["en_us"]["content"].as_array())
                .or_else(|| parsed["content"].as_array());
            let mut refs = Vec::new();
            for paragraph in blocks.into_iter().flatten() {
                for node in paragraph.as_array().into_iter().flatten() {
                    match node["tag"].as_str().unwrap_or("") {
                        "img" => {
                            if let Some(k) = node["image_key"].as_str().filter(|k| !k.is_empty()) {
                                refs.push(MediaRef {
                                    key: k.to_string(),
                                    kind: "image",
                                    name: None,
                                });
                            }
                        }
                        "media" => {
                            if let Some(k) = node["file_key"].as_str().filter(|k| !k.is_empty()) {
                                refs.push(MediaRef {
                                    key: k.to_string(),
                                    kind: "file",
                                    name: node["file_name"].as_str().map(str::to_string),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            refs
        }
        _ => Vec::new(),
    }
}

// ===== Mention handling =====

pub(crate) fn check_bot_mention(mentions: Option<&[serde_json::Value]>, bot_open_id: &str) -> bool {
    let Some(mentions) = mentions else {
        return false;
    };
    if bot_open_id.is_empty() {
        return false;
    }
    mentions.iter().any(|m| {
        m.get("id")
            .and_then(|id| id.get("open_id"))
            .and_then(|v| v.as_str())
            .map_or(false, |oid| oid == bot_open_id)
    })
}

pub(crate) fn remove_bot_mention_placeholders(
    text: &str,
    mentions: Option<&[serde_json::Value]>,
    bot_open_id: &str,
) -> String {
    let Some(mentions) = mentions else {
        return text.to_string();
    };
    if bot_open_id.is_empty() {
        return text.to_string();
    }
    let mut result = text.to_string();
    for m in mentions {
        let is_bot = m
            .get("id")
            .and_then(|id| id.get("open_id"))
            .and_then(|v| v.as_str())
            .map_or(false, |oid| oid == bot_open_id);
        if is_bot {
            if let Some(key) = m.get("key").and_then(|v| v.as_str()) {
                result = result.replace(key, "").trim().to_string();
            }
        }
    }
    result
}

// ===== Message splitting =====

pub(crate) fn split_message(text: &str) -> Vec<String> {
    if text.len() <= FEISHU_MAX_LEN {
        return vec![text.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut remaining = text;
    while remaining.len() > FEISHU_MAX_LEN {
        let chunk = &remaining[..FEISHU_MAX_LEN];
        let split_at = chunk
            .rfind('\n')
            .filter(|&pos| pos > FEISHU_MAX_LEN / 2)
            .unwrap_or(FEISHU_MAX_LEN);
        parts.push(remaining[..split_at].to_string());
        remaining = remaining[split_at..].trim_start_matches('\n');
    }
    if !remaining.is_empty() {
        parts.push(remaining.to_string());
    }
    parts
}

// ===== JID utilities =====

pub(crate) fn jid_to_receive_id_type(jid: &str) -> &'static str {
    if jid.starts_with("feishu:user:") {
        "open_id"
    } else {
        "chat_id"
    }
}

pub(crate) fn jid_to_chat_id(jid: &str) -> Option<&str> {
    jid.strip_prefix("feishu:user:")
        .or_else(|| jid.strip_prefix("feishu:group:"))
}

pub(crate) fn make_jid(chat_type: &str, id: &str) -> String {
    match chat_type {
        "p2p" | "private" => format!("feishu:user:{id}"),
        _ => format!("feishu:group:{id}"),
    }
}

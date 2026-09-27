use std::time::Duration;

use anyhow::{Context, Result};

use super::types::{
    ApiResponse, BotInfoResponse, FeishuBotInfo, SendMessageBody,
};
use super::APP_INIT_TIMEOUT_SECS;

// ===== Bot info API =====

pub(crate) async fn fetch_bot_info(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<FeishuBotInfo> {
    let url = format!("{base_url}/open-apis/bot/v3/info");
    let resp = http
        .get(&url)
        .bearer_auth(token)
        .timeout(Duration::from_secs(APP_INIT_TIMEOUT_SECS))
        .send()
        .await
        .context("fetch bot info")?;

    let body: BotInfoResponse = resp.json().await.context("parse bot info")?;
    if body.code != 0 {
        anyhow::bail!("fetch bot info: code={}, msg={:?}", body.code, body.msg);
    }

    let bot = body.bot.context("bot not found in response")?;
    let open_id = bot["open_id"]
        .as_str()
        .context("bot missing open_id")?
        .to_string();
    let name = bot["bot_name"].as_str().unwrap_or("Feishu Bot").to_string();

    Ok(FeishuBotInfo { open_id, name })
}

// ===== User info API =====

// ===== Feishu API v1 messages =====

pub(crate) async fn call_api(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    path: &str,
    body: &SendMessageBody,
    receive_id_type: &str,
) -> Result<()> {
    let url = format!("{base_url}{path}?receive_id_type={receive_id_type}");
    let resp = http
        .post(&url)
        .bearer_auth(token)
        .json(body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("feishu api call")?;

    let r: ApiResponse = resp.json().await.context("parse api response")?;
    if r.code != 0 {
        anyhow::bail!("feishu api error: code={}, msg={:?}", r.code, r.msg);
    }
    Ok(())
}

/// Largest inbound blob we pull off a message (same cap as a chat upload).
pub(crate) const MAX_RESOURCE_BYTES: usize = crate::agent::documents::MAX_DOC_BYTES;

/// Download one resource attached to a message.
///
/// `kind` is `"image"` for an `image_key` and `"file"` for a `file_key` —
/// including audio and video, which the API also files under `file`. Needs
/// the `im:resource` scope (or `im:message`); without it Feishu answers 403
/// with a JSON body, which is why a JSON content-type is reported as the
/// error it is rather than saved as if it were the picture.
pub(crate) async fn download_message_resource(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    message_id: &str,
    key: &str,
    kind: &str,
) -> Result<Vec<u8>> {
    let url = format!(
        "{base_url}/open-apis/im/v1/messages/{}/resources/{}?type={kind}",
        urlencoding::encode(message_id),
        urlencoding::encode(key)
    );
    let resp = http
        .get(&url)
        .bearer_auth(token)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .context("download message resource")?;
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("download resource HTTP {status}: {body}");
    }
    if content_type.starts_with("application/json") {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("download resource returned an error body: {body}");
    }
    let bytes = resp.bytes().await.context("resource body")?;
    if bytes.len() > MAX_RESOURCE_BYTES {
        anyhow::bail!(
            "resource is {} bytes, over the {MAX_RESOURCE_BYTES} byte limit",
            bytes.len()
        );
    }
    Ok(bytes.to_vec())
}

/// Upload a file, return the file_key.
pub(crate) async fn upload_file(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    file_path: &str,
    file_name: &str,
) -> Result<String> {
    let data = tokio::fs::read(file_path)
        .await
        .context("read upload file")?;
    let part = reqwest::multipart::Part::bytes(data)
        .file_name(file_name.to_string())
        .mime_str("application/octet-stream")?;

    let form = reqwest::multipart::Form::new()
        .text("file_type", "stream")
        .text("file_name", file_name.to_string())
        .part("file", part);

    let url = format!("{base_url}/open-apis/im/v1/files");
    let resp = http
        .post(&url)
        .bearer_auth(token)
        .multipart(form)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .context("upload file to feishu")?;

    let r: ApiResponse = resp.json().await.context("parse upload response")?;
    if r.code != 0 {
        anyhow::bail!("feishu upload error: code={}, msg={:?}", r.code, r.msg);
    }
    r.data
        .as_ref()
        .and_then(|d| d["file_key"].as_str())
        .map(|s| s.to_string())
        .context("missing file_key in upload response")
}

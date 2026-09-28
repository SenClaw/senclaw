//! Internal typed calls into runtime processes — for code running *inside*
//! the daemon (the decision gate, the pre-turn skill router, OCR for
//! text-only models), as opposed to [`super::proxy`], which forwards an
//! external caller's own request byte-for-byte.

use reqwest::{Method, StatusCode};
use sen_runtime_sdk::manifest::Slot;

use super::manager::{RuntimeClientError, RuntimeManager};

async fn call(
    manager: &RuntimeManager,
    slot: Slot,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
    content_type: Option<&str>,
) -> Result<(StatusCode, Vec<u8>), RuntimeClientError> {
    let dial = manager.ensure_slot_started(slot).await?;
    manager.begin_request(&dial.process_key);
    let client = reqwest::Client::new();
    let url = format!("{}{path}", dial.base_url);
    let mut req = client.request(method, &url).bearer_auth(&dial.token);
    if let Some(ct) = content_type {
        req = req.header(reqwest::header::CONTENT_TYPE, ct);
    }
    if let Some(b) = body {
        req = req.body(b);
    }
    let result = req.send().await;
    manager.end_request(&dial.process_key);
    let resp = result.map_err(|e| RuntimeClientError::Upstream(format!("could not reach the runtime: {e}")))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| RuntimeClientError::Upstream(format!("could not read the runtime's answer: {e}")))?;
    Ok((status, bytes.to_vec()))
}

/// A JSON call to the browser runtime (`sen-browser`): `path` is its own
/// route (`/v1/...`). Returns the status and body; the caller reads the
/// runtime's `{error, code}` shape itself.
pub async fn browser_call(
    manager: &RuntimeManager,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<(StatusCode, Vec<u8>), RuntimeClientError> {
    call(manager, Slot::Browser, method, path, body, Some("application/json")).await
}

/// `POST /api/decision/ask` on the decision slot. `request_text` must already
/// be serialized (built from a [`crate::decision::types::AskRequest`] via
/// `serde_json::to_string`, never through `serde_json::Value`) so key order
/// reaches the model unchanged.
pub async fn decision_ask(manager: &RuntimeManager, request_text: String) -> Result<Vec<u8>, RuntimeClientError> {
    let (status, body) = call(
        manager,
        Slot::Decision,
        Method::POST,
        "/api/decision/ask",
        Some(request_text.into_bytes()),
        Some("application/json"),
    )
    .await?;
    if !status.is_success() {
        return Err(RuntimeClientError::Upstream(format!(
            "the decision runtime answered {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    Ok(body)
}

/// `POST /api/ocr/recognize` on the OCR slot. `Ok(None)` means no OCR runtime
/// is installed or selected — the caller's degrade path ("OCR yielded
/// nothing, don't guess"), never a hard failure. Any other problem (installed
/// but would not start, a bad answer) is `Err`.
pub async fn ocr_recognize(
    manager: &RuntimeManager,
    image_bytes: Vec<u8>,
    filename: &str,
) -> Result<Option<String>, RuntimeClientError> {
    let dial = match manager.ensure_slot_started(Slot::Ocr).await {
        Ok(d) => d,
        Err(RuntimeClientError::NotInstalled { .. } | RuntimeClientError::NotSelected { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    manager.begin_request(&dial.process_key);
    let client = reqwest::Client::new();
    let part = reqwest::multipart::Part::bytes(image_bytes).file_name(filename.to_string());
    let form = reqwest::multipart::Form::new().part("image", part);
    let url = format!("{}/api/ocr/recognize", dial.base_url);
    let result = client.post(&url).bearer_auth(&dial.token).multipart(form).send().await;
    manager.end_request(&dial.process_key);
    let resp = result.map_err(|e| RuntimeClientError::Upstream(format!("could not reach the OCR runtime: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(RuntimeClientError::Upstream(format!("the OCR runtime answered {status}: {body}")));
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| RuntimeClientError::Upstream(format!("the OCR runtime sent an unreadable answer: {e}")))?;
    Ok(json.get("text").and_then(|v| v.as_str()).map(str::to_string))
}

//! WebFetch tool — retrieves a URL and returns text/markdown.
//!
//! Equivalent of sema-core's `FetchUrl` (TS) — a side-effecting tool that
//! reaches the network. Subject to permission gating.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolPermissionInfo, ToolResultMessage};

const MAX_BODY_BYTES: usize = 2 * 1024 * 1024; // 2 MiB
const MAX_RETURN_LEN: usize = 60_000;
const REQUEST_TIMEOUT_SECS: u64 = 30;

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return its content as text or markdown. Content over 60 KB is truncated. \
         HTML is stripped to plain text. Use the `prompt` field to remind yourself why you fetched it; \
         the prompt is not passed to a summarizer."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Target URL (http or https)."
                },
                "prompt": {
                    "type": "string",
                    "description": "What you intend to do with the fetched content. Recorded for audit; not used for summarization."
                }
            },
            "required": ["url"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if url.is_empty() {
            return Err("url is required".into());
        }
        match reqwest::Url::parse(url) {
            Ok(u) => {
                let scheme = u.scheme();
                if scheme != "http" && scheme != "https" {
                    return Err(format!("Unsupported scheme: {url}"));
                }
                if crate::util::fetch_safety::is_blocked_fetch_host(url) {
                    return Err(format!(
                        "Blocked URL (SSRF guard): {url} resolves to a local, private, or link-local address"
                    ));
                }
                Ok(())
            }
            Err(_) => Err(format!("Malformed URL: {url}")),
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let url = input
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let started = std::time::Instant::now();
        // Re-check every redirect hop against the SSRF guard so a public URL
        // cannot bounce the request onto localhost or the metadata endpoint.
        let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 5 {
                return attempt.error("too many redirects");
            }
            if crate::util::fetch_safety::is_blocked_fetch_host(attempt.url().as_str()) {
                return attempt.error("redirect to a blocked (local/private) address");
            }
            attempt.follow()
        });
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .redirect(redirect_policy)
            .build()?;

        let resp = match client
            .get(&url)
            .header("User-Agent", "SenClaw/1.0")
            .header(
                "Accept",
                "text/html,text/markdown,text/plain,application/json;q=0.9,*/*;q=0.5",
            )
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("Fetch failed: {e}");
                return Ok(vec![ToolOutput::Result {
                    data: serde_json::json!({"url": url, "error": msg}),
                    result_for_assistant: msg,
                }]);
            }
        };

        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        let body_bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                let msg = format!("Body read failed: {e}");
                return Ok(vec![ToolOutput::Result {
                    data: serde_json::json!({"url": url, "error": msg, "status": status.as_u16()}),
                    result_for_assistant: msg,
                }]);
            }
        };
        let bytes_len = body_bytes.len();
        let truncated = bytes_len > MAX_BODY_BYTES;
        let slice = &body_bytes[..bytes_len.min(MAX_BODY_BYTES)];
        let body_str = String::from_utf8_lossy(slice).to_string();

        let text = if content_type.contains("html") {
            strip_html(&body_str)
        } else {
            body_str
        };

        let final_text = if text.len() > MAX_RETURN_LEN {
            format!(
                "{}\n\n…[truncated, total {} bytes]",
                &text[..MAX_RETURN_LEN],
                bytes_len
            )
        } else if truncated {
            format!("{}\n\n…[body truncated at {} bytes]", text, MAX_BODY_BYTES)
        } else {
            text
        };

        let elapsed_ms = started.elapsed().as_millis() as u64;
        let data = serde_json::json!({
            "url": url,
            "status": status.as_u16(),
            "contentType": content_type,
            "bytes": bytes_len,
            "durationMs": elapsed_ms,
        });

        Ok(vec![ToolOutput::Result {
            data,
            result_for_assistant: final_text,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, input: &Value) -> ToolResultMessage {
        let url = input
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let bytes = data.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
        let status = data.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
        ToolResultMessage {
            title: url,
            summary: format!("{} • {} bytes", status, bytes),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
        match reqwest::Url::parse(url) {
            Ok(u) => u
                .host_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "WebFetch".to_string()),
            Err(_) => "WebFetch".to_string(),
        }
    }

    fn gen_tool_permission(&self, input: &Value) -> Option<ToolPermissionInfo> {
        Some(ToolPermissionInfo {
            title: "Fetch URL".to_string(),
            content: input
                .get("url")
                .cloned()
                .unwrap_or_else(|| Value::String(String::new())),
        })
    }
}

/// Very small HTML→text stripper. Removes `<script>` / `<style>` blocks and
/// reduces tags to whitespace. Not a full HTML parser — good enough for
/// agent consumption of articles, docs, and search results.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut in_tag = false;
    let mut skip_block: Option<&[u8]> = None;
    while i < bytes.len() {
        // Skip script/style blocks
        if let Some(close) = skip_block {
            if bytes[i..].starts_with(close) {
                skip_block = None;
                i += close.len();
                continue;
            }
            i += 1;
            continue;
        }
        if !in_tag && bytes[i] == b'<' {
            // Detect <script ...> or <style ...>
            let lower_rest = html[i..].to_ascii_lowercase();
            if lower_rest.starts_with("<script") {
                skip_block = Some(b"</script>");
                i += 1;
                in_tag = true;
                continue;
            }
            if lower_rest.starts_with("<style") {
                skip_block = Some(b"</style>");
                i += 1;
                in_tag = true;
                continue;
            }
            in_tag = true;
            i += 1;
            continue;
        }
        if in_tag {
            if bytes[i] == b'>' {
                in_tag = false;
                out.push(' ');
            }
            i += 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    // Collapse consecutive whitespace
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_ws = false;
    for ch in out.chars() {
        let is_ws = ch.is_whitespace();
        if is_ws {
            if !prev_ws {
                collapsed.push(if ch == '\n' { '\n' } else { ' ' });
            }
            prev_ws = true;
        } else {
            collapsed.push(ch);
            prev_ws = false;
        }
    }
    decode_entities(collapsed.trim())
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_removes_tags() {
        let html = "<html><body><h1>Hi</h1><p>World</p></body></html>";
        assert_eq!(strip_html(html), "Hi World");
    }

    #[test]
    fn strip_html_drops_scripts() {
        let html = "<p>Before</p><script>alert('x')</script><p>After</p>";
        let out = strip_html(html);
        assert!(out.contains("Before"));
        assert!(out.contains("After"));
        assert!(!out.contains("alert"));
    }

    #[test]
    fn strip_html_decodes_entities() {
        let html = "<p>Tom &amp; Jerry &lt;hi&gt;</p>";
        assert_eq!(strip_html(html), "Tom & Jerry <hi>");
    }

    #[tokio::test]
    async fn validate_rejects_bad_scheme() {
        let tool = WebFetchTool;
        let ctx = ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let bad = serde_json::json!({"url": "ftp://example.com"});
        assert!(tool.validate_input(&bad, &ctx).await.is_err());
        let good = serde_json::json!({"url": "https://example.com"});
        assert!(tool.validate_input(&good, &ctx).await.is_ok());
    }
}

//! `POST /api/ui/open-url` — open an external URL in the host machine's
//! default browser.
//!
//! Space Apps run inside an embedded webview in the desktop app; links must
//! not navigate that webview away from the app UI. The preferred in-page path
//! is the Flutter bridge (`senclawOpenExternal`), with `window.open` as the
//! plain-browser fallback — this endpoint is the third, host-side trigger for
//! programmatic callers (agents, app backends) and UIs that can reach the
//! daemon. Full flow: docs/space-app-open-external.md.
//!
//! The UI server binds 127.0.0.1 only, so the endpoint cannot be driven from
//! another machine. Only http/https URLs are accepted, and the URL is passed
//! to the OS opener as a single argv entry — never through a shell.

use axum::{http::StatusCode, response::Json};
use serde::Deserialize;

use super::core::AppError;

#[derive(Deserialize)]
pub(crate) struct OpenUrlBody {
    url: String,
}

pub(crate) async fn open_url_handler(
    Json(body): Json<OpenUrlBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let url = validate_external_url(&body.url)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    open_in_host_browser(&url)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    tracing::info!("[UIServer] opened external URL in host browser: {url}");
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Accept only absolute http/https URLs with a host part and no control
/// characters. Rejecting everything else keeps `file://`, `javascript:` and
/// custom app schemes from reaching the OS opener.
fn validate_external_url(raw: &str) -> Result<String, &'static str> {
    let url = raw.trim();
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("URL chứa ký tự không hợp lệ");
    }
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .ok_or("chỉ chấp nhận URL http/https tuyệt đối")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return Err("URL thiếu host");
    }
    Ok(url.to_string())
}

/// Hand the URL to the platform opener as one argv entry (no shell parsing).
/// `spawn` + drop: the opener detaches immediately; we don't wait on it.
fn open_in_host_browser(url: &str) -> std::io::Result<()> {
    use std::process::Command;
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // rundll32 avoids cmd.exe metacharacter parsing of `start`.
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(url);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::validate_external_url;

    #[test]
    fn accepts_http_and_https() {
        assert_eq!(
            validate_external_url("https://pnj.com.vn/gia-vang").unwrap(),
            "https://pnj.com.vn/gia-vang"
        );
        assert!(validate_external_url("http://localhost:4570/x").is_ok());
        // Scheme match is case-insensitive but the URL is passed through as-is.
        assert_eq!(
            validate_external_url("  HTTPS://Example.com/A?b=1#c  ").unwrap(),
            "HTTPS://Example.com/A?b=1#c"
        );
    }

    #[test]
    fn rejects_non_http_schemes() {
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://x.com",
            "senclaw://open",
            "//no-scheme.com",
            "example.com/no-scheme",
        ] {
            assert!(validate_external_url(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn rejects_empty_host_and_control_chars() {
        assert!(validate_external_url("https:///path-only").is_err());
        assert!(validate_external_url("https://").is_err());
        assert!(validate_external_url("https://a.com/x y").is_err());
        assert!(validate_external_url("https://a.com/a\tb").is_err());
        // Leading/trailing whitespace is trimmed, not rejected.
        assert!(validate_external_url("https://a.com/\n").is_ok());
    }
}

//! QR Login flow for WeChat iLink Bot.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};

use super::helpers::{MAX_QR_REFRESH, QR_LONG_POLL_TIMEOUT_MS};
use super::types::{QrCodeResponse, QrStatusResponse};

pub(crate) struct QrLoginResult {
    pub(crate) token: String,
    pub(crate) base_url: Option<String>,
    pub(crate) user_id: Option<String>,
}

pub(crate) async fn run_qr_login(
    http: &reqwest::Client,
    api_base_url: &str,
) -> Result<QrLoginResult> {
    let base = if api_base_url.ends_with('/') {
        api_base_url.to_string()
    } else {
        format!("{api_base_url}/")
    };

    // Fetch QR code
    let qr_url = format!("{base}ilink/bot/get_bot_qrcode?bot_type=3");
    let resp = http
        .get(&qr_url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .context("fetch QR code")?;
    let mut qr_data: QrCodeResponse = resp.json().await.context("parse QR code response")?;

    crate::safe_println!("\n[WeChatChannel] Please scan the QR code with WeChat to log in:");
    // Print QR code to terminal if possible
    if let Ok(qr_img) = qrcode::QrCode::new(&qr_data.qrcode_img_content) {
        let rendered: String = qr_img
            .render::<char>()
            .quiet_zone(false)
            .module_dimensions(2, 1)
            .build();
        for line in rendered.split('\n') {
            crate::safe_println!("  {line}");
        }
    }
    crate::safe_println!("  {}\n", qr_data.qrcode_img_content);

    let mut refresh_count = 0u32;
    let mut scanned_printed = false;

    loop {
        let status_url = format!(
            "{}ilink/bot/get_qrcode_status?qrcode={}",
            base, qr_data.qrcode
        );
        let resp = http
            .get(&status_url)
            .header("iLink-App-ClientVersion", "1")
            .timeout(Duration::from_millis(QR_LONG_POLL_TIMEOUT_MS))
            .send()
            .await
            .context("poll QR status")?;

        let status: QrStatusResponse = resp.json().await.context("parse QR status")?;

        match status.status.as_str() {
            "wait" => {
                eprint!(".");
                std::io::stderr().flush().ok();
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            "scaned" => {
                if !scanned_printed {
                    crate::safe_println!(
                        "\n[WeChatChannel] QR scanned, please confirm in WeChat..."
                    );
                    scanned_printed = true;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            "expired" => {
                refresh_count += 1;
                if refresh_count > MAX_QR_REFRESH {
                    anyhow::bail!("QR code expired multiple times, please restart login flow");
                }
                crate::safe_println!(
                    "\n[WeChatChannel] QR code expired, refreshing ({refresh_count}/{})...",
                    MAX_QR_REFRESH
                );
                // Refresh QR
                let resp2 = http
                    .get(&qr_url)
                    .timeout(Duration::from_secs(15))
                    .send()
                    .await
                    .context("refresh QR code")?;
                qr_data = resp2.json().await.context("parse refreshed QR")?;
                if let Ok(qr_img) = qrcode::QrCode::new(&qr_data.qrcode_img_content) {
                    let rendered: String = qr_img
                        .render::<char>()
                        .quiet_zone(false)
                        .module_dimensions(2, 1)
                        .build();
                    for line in rendered.split('\n') {
                        crate::safe_println!("  {line}");
                    }
                }
                crate::safe_println!("  {}\n", qr_data.qrcode_img_content);
                scanned_printed = false;
            }
            "confirmed" => {
                let _ilink_bot_id = status
                    .ilink_bot_id
                    .ok_or_else(|| anyhow::anyhow!("missing ilink_bot_id"))?;
                let token = status
                    .bot_token
                    .ok_or_else(|| anyhow::anyhow!("missing bot_token"))?;
                crate::safe_println!("\n[WeChatChannel] WeChat login successful!");
                if let Some(ref uid) = status.ilink_user_id {
                    crate::safe_println!("[WeChatChannel] Bound user: {uid}");
                }
                return Ok(QrLoginResult {
                    token,
                    base_url: status.baseurl.or(Some(api_base_url.to_string())),
                    user_id: status.ilink_user_id,
                });
            }
            _ => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

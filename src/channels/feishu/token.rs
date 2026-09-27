use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use super::helpers::now_secs;
use super::types::{CachedToken, TenantTokenRequest, TenantTokenResponse};
use super::TOKEN_REFRESH_MARGIN_SECS;

// ===== Token management =====

pub(crate) async fn get_tenant_access_token(
    http: &reqwest::Client,
    base_url: &str,
    app_id: &str,
    app_secret: &str,
) -> Result<CachedToken> {
    let url = format!("{base_url}/open-apis/auth/v3/tenant_access_token/internal");
    let resp = http
        .post(&url)
        .json(&TenantTokenRequest { app_id, app_secret })
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("fetch tenant_access_token")?;

    let body: TenantTokenResponse = resp.json().await.context("parse token response")?;
    if body.code != 0 {
        anyhow::bail!(
            "tenant_access_token failed: code={}, msg={:?}",
            body.code,
            body.msg
        );
    }
    let token = body
        .tenant_access_token
        .context("missing token in response")?;
    let expire = body.expire.unwrap_or(7200);
    let expires_at = now_secs() + expire - TOKEN_REFRESH_MARGIN_SECS;
    Ok(CachedToken { token, expires_at })
}

// ===== Token helpers =====

pub(crate) async fn get_or_refresh_token(
    http: &reqwest::Client,
    base_url: &str,
    app_id: &str,
    app_secret: &str,
    tokens: &Mutex<HashMap<String, CachedToken>>,
) -> Result<String> {
    {
        let tokens = tokens.lock().await;
        if let Some(cached) = tokens.get(app_id) {
            if now_secs() < cached.expires_at {
                return Ok(cached.token.clone());
            }
        }
    }

    let cached = get_tenant_access_token(http, base_url, app_id, app_secret).await?;
    let token = cached.token.clone();
    {
        let mut tokens = tokens.lock().await;
        tokens.insert(app_id.to_string(), cached);
    }
    Ok(token)
}

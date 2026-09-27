//! ClawHub API client. Mirrors `src-old/clawhub/client.ts`.
//!
//! Wraps the clawhub.ai public API (search/install/update).
//! No login token needed for read operations.

use std::time::Duration;

use anyhow::{bail, Context};
use reqwest::multipart;
use serde::{Deserialize, Serialize};

use crate::clawhub::auth::read_stored_token;

pub const DEFAULT_REGISTRY: &str = "https://lightmake.site";
const REQUEST_TIMEOUT_SECS: u64 = 15;

// ===== Response types =====

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub score: f64,
    pub slug: String,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    pub summary: Option<String>,
    pub version: Option<String>,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillMetaSkill {
    pub slug: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub summary: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(rename = "updatedAt")]
    pub updated_at: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillMetaVersion {
    pub version: String,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    pub changelog: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModerationInfo {
    #[serde(rename = "isSuspicious")]
    pub is_suspicious: bool,
    #[serde(rename = "isMalwareBlocked")]
    pub is_malware_blocked: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillMeta {
    pub skill: Option<SkillMetaSkill>,
    #[serde(rename = "latestVersion")]
    pub latest_version: Option<SkillMetaVersion>,
    pub moderation: Option<ModerationInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveResult {
    pub r#match: Option<ResolveMatch>,
    #[serde(rename = "latestVersion")]
    pub latest_version: Option<ResolveMatch>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveMatch {
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WhoamiResult {
    pub handle: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    pub image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishResult {
    pub slug: String,
    pub version: String,
    #[serde(rename = "createdAt")]
    pub created_at: Option<u64>,
}

// ===== Helpers =====

async fn resolve_token(explicit: Option<&str>) -> Option<String> {
    if let Some(t) = explicit {
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    read_stored_token()
}

fn registry_url(path: &str, registry: &str) -> String {
    let base = if registry.ends_with('/') {
        registry.to_string()
    } else {
        format!("{registry}/")
    };
    let rel = path.strip_prefix('/').unwrap_or(path);
    format!("{base}{rel}")
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .build()
        .expect("reqwest client")
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    url: &str,
    token: Option<&str>,
) -> Result<T, anyhow::Error> {
    let mut req = http_client().get(url).header("Accept", "application/json");
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.context("HTTP request failed")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("HTTP {status}: {}", &body[..body.len().min(200)]);
    }
    resp.json().await.context("invalid JSON response")
}

async fn fetch_binary(url: &str, token: Option<&str>) -> Result<Vec<u8>, anyhow::Error> {
    let mut req = http_client().get(url);
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await.context("HTTP request failed")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("HTTP {status} downloading zip");
    }
    resp.bytes().await.map(|b| b.to_vec()).context("read body")
}

// ===== Public API =====

pub async fn search_skills(
    query: &str,
    registry: Option<&str>,
    limit: Option<u32>,
    token: Option<&str>,
) -> Result<Vec<SearchResult>, anyhow::Error> {
    let registry = registry.unwrap_or(DEFAULT_REGISTRY);
    let token = resolve_token(token).await;
    let base = registry_url("/api/v1/search", registry);

    let mut url = reqwest::Url::parse(&base)?;
    url.query_pairs_mut().append_pair("q", query);
    if let Some(n) = limit {
        url.query_pairs_mut().append_pair("limit", &n.to_string());
    }

    #[derive(Deserialize)]
    struct SearchResponse {
        results: Vec<SearchResult>,
    }

    let resp: SearchResponse = fetch_json(url.as_str(), token.as_deref()).await?;
    Ok(resp.results)
}

pub async fn get_skill_meta(
    slug: &str,
    registry: Option<&str>,
    token: Option<&str>,
) -> Result<SkillMeta, anyhow::Error> {
    let registry = registry.unwrap_or(DEFAULT_REGISTRY);
    let token = resolve_token(token).await;
    let url = registry_url(&format!("/api/v1/skills/{}", urlencoding(slug)), registry);
    fetch_json(&url, token.as_deref()).await
}

fn urlencoding(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

pub async fn download_skill_zip(
    slug: &str,
    version: &str,
    registry: Option<&str>,
    token: Option<&str>,
) -> Result<Vec<u8>, anyhow::Error> {
    let registry = registry.unwrap_or(DEFAULT_REGISTRY);
    let token = resolve_token(token).await;
    let base = registry_url("/api/v1/download", registry);
    let mut url = reqwest::Url::parse(&base)?;
    url.query_pairs_mut().append_pair("slug", slug);
    url.query_pairs_mut().append_pair("version", version);
    fetch_binary(url.as_str(), token.as_deref()).await
}

pub async fn resolve_skill_version(
    slug: &str,
    fingerprint: &str,
    registry: Option<&str>,
    token: Option<&str>,
) -> Result<ResolveResult, anyhow::Error> {
    let registry = registry.unwrap_or(DEFAULT_REGISTRY);
    let token = resolve_token(token).await;
    let base = registry_url("/api/v1/resolve", registry);
    let mut url = reqwest::Url::parse(&base)?;
    url.query_pairs_mut().append_pair("slug", slug);
    url.query_pairs_mut().append_pair("hash", fingerprint);
    fetch_json(url.as_str(), token.as_deref()).await
}

pub async fn whoami(
    registry: Option<&str>,
    token: Option<&str>,
) -> Result<WhoamiResult, anyhow::Error> {
    let registry = registry.unwrap_or(DEFAULT_REGISTRY);
    let token = resolve_token(token).await;
    let token =
        token.ok_or_else(|| anyhow::anyhow!("Not logged in. Run: senclaw clawhub login"))?;
    let url = registry_url("/api/v1/whoami", registry);

    #[derive(Deserialize)]
    struct WhoamiResponse {
        user: WhoamiResult,
    }

    let resp: WhoamiResponse = fetch_json(&url, Some(&token)).await?;
    Ok(resp.user)
}

pub async fn publish_skill(
    slug: &str,
    display_name: &str,
    version: &str,
    changelog: &str,
    tags: &[String],
    files: Vec<(String, Vec<u8>)>,
    registry: Option<&str>,
    token: Option<&str>,
) -> Result<PublishResult, anyhow::Error> {
    let registry = registry.unwrap_or(DEFAULT_REGISTRY);
    let token = resolve_token(token).await;
    let token =
        token.ok_or_else(|| anyhow::anyhow!("Not logged in. Run: senclaw clawhub login"))?;
    let url = registry_url("/api/v1/skills", registry);

    let payload = serde_json::json!({
        "slug": slug,
        "displayName": display_name,
        "version": version,
        "changelog": changelog,
        "tags": tags,
    });

    let mut form = multipart::Form::new().text("payload", payload.to_string());

    for (name, data) in files {
        let part = multipart::Part::bytes(data)
            .file_name(name)
            .mime_str("application/octet-stream")?;
        form = form.part("files", part);
    }

    #[derive(Deserialize)]
    struct PublishResponse {
        skill: PublishResult,
    }

    let resp: PublishResponse = http_client()
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .multipart(form)
        .send()
        .await
        .context("publish request failed")?
        .json()
        .await
        .context("invalid publish response")?;

    Ok(resp.skill)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_url() {
        assert_eq!(
            registry_url("/api/v1/search", "https://example.com"),
            "https://example.com/api/v1/search"
        );
        assert_eq!(
            registry_url("/api/v1/search", "https://example.com/"),
            "https://example.com/api/v1/search"
        );
        assert_eq!(
            registry_url("api/v1/search", "https://example.com"),
            "https://example.com/api/v1/search"
        );
        assert_eq!(
            registry_url("/api/v1/skills/my-skill", "https://example.com"),
            "https://example.com/api/v1/skills/my-skill"
        );
    }

    #[test]
    fn test_urlencoding() {
        assert_eq!(urlencoding("hello"), "hello");
        assert_eq!(urlencoding("my-skill"), "my-skill");
        assert_eq!(urlencoding("hello world"), "hello%20world");
        assert_eq!(urlencoding("a/b"), "a%2Fb");
    }

    #[test]
    fn test_default_registry_is_set() {
        assert!(!DEFAULT_REGISTRY.is_empty());
    }
}

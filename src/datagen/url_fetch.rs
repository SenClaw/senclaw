//! Downloading a dataset file from a URL someone gave, without letting that
//! URL reach this machine or its network.
//!
//! `util::fetch_safety` judges a URL by its host name, which a name that
//! *resolves* to `127.0.0.1` passes — and the daemon's own API answers a
//! loopback caller with no token. So the client here resolves names itself
//! and refuses any address the same rule blocks; reqwest connects to the
//! address that was checked, which also closes the window a rebinding DNS
//! answer would use. Literal IPs never reach a resolver, so they, and every
//! redirect hop, go through `is_blocked_fetch_host` as well.

use std::net::SocketAddr;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::util::fetch_safety::is_blocked_fetch_host;

/// Largest download; a dataset file, not a corpus dump.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

/// What was downloaded.
pub struct Fetched {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    /// The URL after redirects.
    pub final_url: String,
}

/// Is this address one a fetch must not reach?
fn blocked_addr(addr: &SocketAddr) -> bool {
    let host = match addr {
        SocketAddr::V4(a) => a.ip().to_string(),
        SocketAddr::V6(a) => format!("[{}]", a.ip()),
    };
    is_blocked_fetch_host(&format!("http://{host}/"))
}

struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = addrs.into_iter().filter(|a| !blocked_addr(a)).collect();
            if public.is_empty() {
                return Err(format!("{host} resolves only to a private or local address").into());
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

/// `Ok(())` when the URL may be fetched at all; the reason otherwise.
pub fn check(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| format!("\"{url}\" is not a URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("only http and https URLs can be imported".into());
    }
    if is_blocked_fetch_host(url) {
        return Err("that URL points at this machine or a private network, which an import never reads".into());
    }
    Ok(())
}

/// GET `url`, at most [`MAX_BYTES`].
pub async fn fetch(url: &str) -> Result<Fetched, String> {
    check(url)?;
    let client = reqwest::Client::builder()
        .dns_resolver(std::sync::Arc::new(PublicOnly))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if is_blocked_fetch_host(attempt.url().as_str()) {
                attempt.error("redirected to this machine or a private network")
            } else {
                attempt.follow()
            }
        }))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .user_agent(concat!("SenClaw-datagen/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client.get(url).send().await.map_err(|e| format!("cannot download {url}: {}", error_chain(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{url} answered {status}"));
    }
    if resp.content_length().is_some_and(|n| n as usize > MAX_BYTES) {
        return Err(format!("the file is larger than {} MB", MAX_BYTES / 1024 / 1024));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
    let final_url = resp.url().to_string();
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("download of {url} broke off: {e}"))? {
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err(format!("the file is larger than {} MB", MAX_BYTES / 1024 / 1024));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Fetched { bytes, content_type, final_url })
}

/// reqwest's top-level message hides the resolver's refusal one level down.
fn error_chain(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_http_urls_pass() {
        assert!(check("https://huggingface.co/datasets/x/resolve/main/a.jsonl").is_ok());
        assert!(check("file:///etc/passwd").unwrap_err().contains("http"));
        assert!(check("http://127.0.0.1:18788/api/llm-config").unwrap_err().contains("this machine"));
        assert!(check("http://192.168.0.93/x.csv").is_err());
        assert!(check("not a url").is_err());
    }

    #[test]
    fn resolved_addresses_are_judged_like_literals() {
        assert!(blocked_addr(&"127.0.0.1:0".parse().unwrap()));
        assert!(blocked_addr(&"[::1]:0".parse().unwrap()));
        assert!(blocked_addr(&"10.1.2.3:0".parse().unwrap()));
        assert!(!blocked_addr(&"104.16.0.1:443".parse().unwrap()));
    }

    #[tokio::test]
    async fn a_name_that_resolves_to_loopback_is_refused() {
        let err = PublicOnly.resolve("localhost".parse().unwrap()).await.err().unwrap();
        assert!(err.to_string().contains("private or local"), "{err}");
    }
}

//! Reading the senclaw hub package registry — the install side of
//! [`super::publish`].
//!
//! The hub speaks two different languages on the same host, and they are not
//! interchangeable:
//!
//! * `/marketplace.json` — a Claude Code style *plugin catalog* pointing at git
//!   repos. Handled by [`super::hub`].
//! * `/api/v1/packages/{scope}/{name}` — the *package registry*: versioned
//!   artifacts uploaded by `senclaw hub publish`, each with a platform, a size
//!   and an integrity digest. Handled here.
//!
//! A published version is immutable, so the digest in the document is the
//! authority: a download whose SHA-512 does not match is a corrupted or swapped
//! artifact and is refused rather than installed.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The `Accept` value the hub uses to serve the install document.
const INSTALL_ACCEPT: &str = "application/vnd.senclaw.install-v1+json";

/// Scope assumed when the user types a bare package name.
pub const DEFAULT_SCOPE: &str = "senclaw";

/// One downloadable artifact of one version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistEntry {
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub libc: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    /// `sha512-<base64>`; verified after download.
    #[serde(default)]
    pub integrity: Option<String>,
    pub tarball: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionDoc {
    pub version: String,
    #[serde(default)]
    pub yanked: bool,
    #[serde(default)]
    pub deprecated: Option<String>,
    #[serde(default, rename = "publishedAt")]
    pub published_at: Option<i64>,
    #[serde(default)]
    pub dist: Vec<DistEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageDoc {
    pub slug: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "repoUrl")]
    pub repo_url: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default, rename = "distTags")]
    pub dist_tags: HashMap<String, String>,
    #[serde(default)]
    pub versions: HashMap<String, VersionDoc>,
}

impl PackageDoc {
    /// The version a bare `install` should take.
    pub fn latest(&self) -> Option<&VersionDoc> {
        self.dist_tags
            .get("latest")
            .and_then(|v| self.versions.get(v))
    }

    /// Versions newest-first by publish time, falling back to string order for
    /// documents that predate `publishedAt`.
    pub fn versions_sorted(&self) -> Vec<&VersionDoc> {
        let mut all: Vec<&VersionDoc> = self.versions.values().collect();
        all.sort_by(|a, b| {
            b.published_at
                .cmp(&a.published_at)
                .then_with(|| b.version.cmp(&a.version))
        });
        all
    }
}

/// Split `scope/name`, `@scope/name` or a bare `name` into its parts.
pub fn parse_slug(slug: &str) -> Result<(String, String)> {
    let slug = slug.trim().trim_start_matches('@');
    let parts: Vec<&str> = slug.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [name] => Ok((DEFAULT_SCOPE.to_string(), (*name).to_string())),
        [scope, name] => Ok(((*scope).to_string(), (*name).to_string())),
        _ => bail!("`{slug}` không phải slug hợp lệ (dạng <scope>/<name> hoặc <name>)"),
    }
}

/// Identifies this process to the hub as a SenClaw client, not a browser.
///
/// The hub does not serve *unsigned* app artifacts to anonymous link-following
/// requests — its remediation after Google's Safe Browsing crawler fetched
/// unsigned macOS binaries and flagged the domain
/// (`apps/web/src/app/dl/…/route.ts`). A crawler sends no custom headers, so
/// this header is what separates an install from a sweep. Without it every app
/// in the store 404s on download.
///
/// This is not a credential and grants nothing: private packages still require
/// a real token below.
const CLIENT_HEADER: &str = "x-senclaw-client";

/// The publish token, when the machine has one, reused as a *read* credential
/// so that a private package the user may reach installs without a second
/// login. Absence is normal — a public install needs no token.
fn hub_auth() -> Option<String> {
    super::publish::read_token().ok()
}

/// Identify the client, and authenticate when we can.
fn with_auth(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let req = req.header(
        CLIENT_HEADER,
        concat!("senclaw/", env!("CARGO_PKG_VERSION")),
    );
    match hub_auth() {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

/// Fetch a package document. A 404 is reported as "not found" rather than a
/// bare HTTP code, because that is the one failure users hit by typo.
pub async fn fetch_package(hub: &str, scope: &str, name: &str) -> Result<PackageDoc> {
    let url = format!(
        "{}/api/v1/packages/{scope}/{name}",
        hub.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let resp = with_auth(client.get(&url).header("accept", INSTALL_ACCEPT))
        .send()
        .await
        .with_context(|| format!("không gọi được {url}"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("không có gói `{scope}/{name}` trên hub");
    }
    if !resp.status().is_success() {
        bail!("{url} trả về HTTP {}", resp.status());
    }
    resp.json()
        .await
        .with_context(|| format!("{url} trả về JSON không đúng dạng"))
}

/// Pick the artifact for this machine.
///
/// A platform-less entry (`na`/`any`) is portable and matches anything; anything
/// else must match the host, because installing a darwin-arm64 binary on Linux
/// fails later and more confusingly than failing here.
///
/// The match is on canonical ids, not raw strings: `windows-x64` and `win32-x64`
/// name the same binary, and a spelling difference between what the host reports
/// and what the registry stored must not read as "no build for your machine".
pub fn select_dist<'a>(version: &'a VersionDoc, host: &str) -> Result<&'a DistEntry> {
    use super::publish::canonical_platform;

    let portable = |p: &Option<String>| {
        matches!(
            p.as_deref().map(str::trim),
            None | Some("") | Some("na") | Some("any") | Some("all")
        )
    };

    let want = canonical_platform(host);
    if let Some(exact) = version
        .dist
        .iter()
        .find(|d| d.platform.as_deref().map(canonical_platform).as_deref() == Some(want.as_str()))
    {
        return Ok(exact);
    }
    if let Some(any) = version.dist.iter().find(|d| portable(&d.platform)) {
        return Ok(any);
    }
    if version.dist.is_empty() {
        bail!("phiên bản {} không có artifact nào", version.version);
    }
    bail!(
        "phiên bản {} không có bản cho `{want}` (đang có: {})",
        version.version,
        version
            .dist
            .iter()
            .filter_map(|d| d.platform.clone())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Download an artifact and check it against the registry's digest.
pub async fn download_verified(dist: &DistEntry) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let resp = with_auth(client.get(&dist.tarball))
        .send()
        .await
        .with_context(|| format!("không tải được {}", dist.tarball))?;
    // A 404 on an artifact the catalog just advertised is almost never a missing
    // file — the hub answers 404 rather than 403 so that a package the requester
    // cannot reach is indistinguishable from one that does not exist. Say that,
    // instead of echoing a status code that reads as "the file is gone".
    if resp.status() == reqwest::StatusCode::NOT_FOUND && hub_auth().is_none() {
        bail!(
            "hub không cho tải {} (404).\n\
             Với gói riêng tư thì 404 nghĩa là chưa có quyền — chạy `senclaw hub login` \
             (dán token snc_pat_… lấy ở trang hub → Settings → Tokens) hoặc đặt \
             SENCLAW_HUB_TOKEN, rồi cài lại. Gói công khai thì kiểm tra lại phiên bản \
             và nền tảng có thật trên hub không.",
            dist.tarball
        );
    }
    if !resp.status().is_success() {
        bail!("{} trả về HTTP {}", dist.tarball, resp.status());
    }
    let bytes = resp.bytes().await?.to_vec();

    if let Some(expected) = dist.integrity.as_deref().filter(|s| !s.is_empty()) {
        verify_integrity(&bytes, expected)?;
    }
    if let Some(size) = dist.size {
        if size != bytes.len() as u64 {
            bail!(
                "kích thước tải về ({}) khác với công bố ({size}) — tải lại hoặc báo cho chủ gói",
                bytes.len()
            );
        }
    }
    Ok(bytes)
}

/// Compare bytes against a `sha512-<base64>` string.
pub fn verify_integrity(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = super::publish::sha512_integrity(bytes);
    let expected = expected.trim();
    if !expected.starts_with("sha512-") {
        // Do not silently accept a digest we cannot check: an unknown algorithm
        // is a reason to stop, not to trust.
        bail!("integrity `{expected}` dùng thuật toán lạ — không xác minh được, dừng lại");
    }
    if actual != expected {
        bail!(
            "integrity không khớp — tệp tải về không phải bản đã publish.\n  \
             mong đợi: {expected}\n  thực tế:  {actual}"
        );
    }
    Ok(())
}

/// Resolve a requested version (or `latest`) into an installable document.
pub fn resolve_version<'a>(pkg: &'a PackageDoc, want: Option<&str>) -> Result<&'a VersionDoc> {
    let doc = match want {
        Some(v) => pkg
            .versions
            .get(v)
            .ok_or_else(|| anyhow!("gói {} không có phiên bản {v}", pkg.slug))?,
        None => pkg
            .latest()
            .or_else(|| pkg.versions_sorted().into_iter().next())
            .ok_or_else(|| anyhow!("gói {} chưa publish phiên bản nào", pkg.slug))?,
    };
    if doc.yanked {
        bail!(
            "{}@{} đã bị yank (gỡ) — chọn phiên bản khác",
            pkg.slug,
            doc.version
        );
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> PackageDoc {
        serde_json::from_str(
            r#"{
              "slug": "senclaw/ai-office",
              "kind": "app",
              "description": "demo",
              "owner": "senclaw",
              "distTags": { "latest": "1.0.0" },
              "versions": {
                "1.0.0": {
                  "version": "1.0.0", "yanked": false, "deprecated": null,
                  "publishedAt": 1784563350872,
                  "dist": [{
                    "platform": "darwin-arm64", "libc": "na", "format": "zip",
                    "channel": "stable", "filename": "ai-office-app.zip",
                    "size": 3220752, "integrity": "sha512-AAA",
                    "tarball": "https://hub-store.bacnd.com/dl/senclaw/ai-office/1.0.0/darwin-arm64/ai-office-app.zip"
                  }]
                },
                "0.9.0": {
                  "version": "0.9.0", "yanked": true, "publishedAt": 1784000000000, "dist": []
                }
              }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_the_live_package_document() {
        let d = doc();
        assert_eq!(d.kind.as_deref(), Some("app"));
        assert_eq!(d.latest().unwrap().version, "1.0.0");
        assert_eq!(d.versions_sorted()[0].version, "1.0.0");
    }

    #[test]
    fn slugs_default_to_the_senclaw_scope() {
        assert_eq!(
            parse_slug("ai-office").unwrap(),
            ("senclaw".into(), "ai-office".into())
        );
        assert_eq!(
            parse_slug("acme/thing").unwrap(),
            ("acme".into(), "thing".into())
        );
        assert_eq!(
            parse_slug("@acme/thing").unwrap(),
            ("acme".into(), "thing".into())
        );
        assert!(parse_slug("a/b/c").is_err());
    }

    #[test]
    fn resolves_latest_and_refuses_a_yanked_version() {
        let d = doc();
        assert_eq!(resolve_version(&d, None).unwrap().version, "1.0.0");
        assert_eq!(resolve_version(&d, Some("1.0.0")).unwrap().version, "1.0.0");
        assert!(resolve_version(&d, Some("0.9.0")).is_err());
        assert!(resolve_version(&d, Some("2.0.0")).is_err());
    }

    #[test]
    fn picks_the_host_artifact_and_names_the_alternatives() {
        let d = doc();
        let v = d.latest().unwrap();
        assert_eq!(select_dist(v, "darwin-arm64").unwrap().size, Some(3220752));

        let err = select_dist(v, "linux-x64").unwrap_err().to_string();
        assert!(err.contains("darwin-arm64"), "{err}");
    }

    #[test]
    fn a_windows_host_matches_the_hubs_win32_artifact() {
        // The hub stores `win32-x64`; a Rust host that reports `windows-x64`
        // must still resolve to it instead of "no build for your machine".
        let v: VersionDoc = serde_json::from_str(
            r#"{"version":"1.0.0","dist":[{"platform":"win32-x64","tarball":"http://x/a.zip"}]}"#,
        )
        .unwrap();
        assert!(select_dist(&v, "windows-x64").is_ok());
        assert!(select_dist(&v, "x86_64-pc-windows-msvc").is_ok());
        assert!(select_dist(&v, "linux-x64").is_err());
    }

    #[test]
    fn a_portable_artifact_matches_any_host() {
        let v: VersionDoc = serde_json::from_str(
            r#"{"version":"1.0.0","dist":[{"platform":"na","tarball":"http://x/a.zip"}]}"#,
        )
        .unwrap();
        assert!(select_dist(&v, "linux-x64").is_ok());
    }

    #[test]
    fn integrity_mismatch_is_refused_rather_than_installed() {
        let good = super::super::publish::sha512_integrity(b"hello");
        assert!(verify_integrity(b"hello", &good).is_ok());
        assert!(verify_integrity(b"tampered", &good).is_err());
        // An algorithm we cannot check must fail closed.
        assert!(verify_integrity(b"hello", "sha1-abc").is_err());
    }
}

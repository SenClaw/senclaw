//! Publishing packages to the senclaw hub.
//!
//! The server contract is `POST /api/v1/publish`, `multipart/form-data`, with
//! `Authorization: Bearer snc_pat_…` (hub `apps/web/src/app/api/v1/publish/route.ts`).
//! Three server-side invariants shape this client:
//!
//! * **`name@version` is immutable.** A published version can be yanked or
//!   deprecated but never replaced with different bytes, so the version must be
//!   bumped for every shipped change. We check the version is free *before*
//!   uploading megabytes, because the server answers 409 only after the whole
//!   body has arrived.
//! * **The scope is forced to the token owner's handle.** A client cannot
//!   publish into someone else's namespace, so we never send one.
//! * **The digest is computed server-side** by streaming the stored object. We
//!   compute SHA-512 too, but only to show the user what they are shipping —
//!   sending an integrity value would be theatre, since the server does not
//!   trust it.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// Matches `MAX_WEB_UPLOAD` in the hub (`apps/web/src/server/publish.ts`).
/// Checked locally so a 20 MB+ artifact fails in a second instead of after a
/// long upload.
pub const MAX_UPLOAD_BYTES: u64 = 20 * 1024 * 1024;

pub const DEFAULT_HUB: &str = super::hub::DEFAULT_HUB_URL;

/// Hub metadata for a package, kept beside the app's runtime manifest.
///
/// Deliberately *not* merged into `senclaw-manifest.json`: that file describes
/// how the daemon runs the app, this one describes how the registry lists it.
/// The two have different audiences and different lifetimes, and duplicating
/// the description into both is how they drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubPackage {
    /// Semver. The hub rejects anything else, and rejects a version that exists.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// What the package expects to touch. This is a security declaration shown
    /// to users before install — a wrong one is worse than none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    /// `none` | `tauri` | `electron`.
    #[serde(default = "default_updater")]
    pub updater: String,
    /// Target triple of the artifact, e.g. `darwin-arm64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Relative path to the artifact; defaults to `<name>-app.zip`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

fn default_updater() -> String {
    "none".to_string()
}

pub const HUB_FILE: &str = "senclaw-hub.json";

/// Semver, strict enough to match what the hub accepts.
///
/// `1.0` and `v1.0.0` are the two mistakes people actually make; both are
/// rejected here with a message rather than by a 400 after an upload.
pub fn is_semver(v: &str) -> bool {
    let core = v.split(['-', '+']).next().unwrap_or_default();
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars().all(|c| c.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
        })
}

/// Increment a semver. `part` is `major` | `minor` | `patch`.
pub fn bump(version: &str, part: &str) -> Result<String> {
    if !is_semver(version) {
        bail!("`{version}` không phải semver hợp lệ (cần dạng X.Y.Z)");
    }
    let core = version.split(['-', '+']).next().unwrap_or_default();
    let n: Vec<u64> = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    Ok(match part {
        "major" => format!("{}.0.0", n[0] + 1),
        "minor" => format!("{}.{}.0", n[0], n[1] + 1),
        "patch" => format!("{}.{}.{}", n[0], n[1], n[2] + 1),
        other => bail!("`{other}` không hợp lệ — dùng major | minor | patch"),
    })
}

/// Detect the current host triple, used as the artifact platform default.
///
/// The hub's canonical id is Node's `${process.platform}-${process.arch}`, so
/// every name Rust spells differently is translated here — including `windows`,
/// which Node calls `win32`. Getting that one wrong is not cosmetic: the hub
/// stores `win32-x64`, so a client asking for `windows-x64` finds no artifact
/// and every Windows install 404s.
pub fn host_platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// Any reasonable spelling of a platform → the hub's canonical id.
///
/// Mirrors `normalizePlatform` in the hub's packages/schemas: a build script
/// naming its target `windows-x64` (electron-builder), `darwin-aarch64` (Tauri)
/// or `x86_64-pc-windows-msvc` (cargo) means the same binary we call
/// `win32-x64`. The hub normalises on publish, so stored values are canonical —
/// this exists so a *client* comparison never fails on spelling alone, whatever
/// an older hub or a hand-written manifest happens to contain.
///
/// Unrecognised input comes back trimmed and lowercased rather than empty: an
/// exact match on an unknown-but-identical spelling still beats no match.
pub fn canonical_platform(input: &str) -> String {
    let s = input.trim().to_lowercase();
    if s.is_empty() {
        return s;
    }
    if s == "any" || s == "all" || s == "universal" || s == "na" {
        return "any".to_string();
    }

    // Longest first: `x86_64` must win over `x64`, and `win32` over `win`.
    const OS_ALIASES: &[(&str, &str)] = &[
        ("darwin", "darwin"),
        ("macos", "darwin"),
        ("osx", "darwin"),
        ("apple", "darwin"),
        ("mac", "darwin"),
        ("windows", "win32"),
        ("win32", "win32"),
        ("win64", "win32"),
        ("win", "win32"),
        ("linux", "linux"),
    ];
    const ARCH_ALIASES: &[(&str, &str)] = &[
        ("aarch64", "arm64"),
        ("x86_64", "x64"),
        ("x86-64", "x64"),
        ("amd64", "x64"),
        ("arm64", "arm64"),
        ("x64", "x64"),
        ("arm", "arm64"),
    ];

    let os = OS_ALIASES.iter().find(|(a, _)| s.contains(a)).map(|(_, c)| *c);
    let arch = ARCH_ALIASES.iter().find(|(a, _)| s.contains(a)).map(|(_, c)| *c);
    match (os, arch) {
        (Some(os), Some(arch)) => format!("{os}-{arch}"),
        _ => s,
    }
}

/// Where the publish token lives. Never passed on the command line — an
/// argument would land in shell history and in every process listing.
pub fn token_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".senclaw")
        .join("hub-token")
}

/// Read the token from `SENCLAW_HUB_TOKEN` or the token file.
pub fn read_token() -> Result<String> {
    if let Ok(t) = std::env::var("SENCLAW_HUB_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let path = token_path();
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "chưa đăng nhập hub. Chạy `senclaw hub login` (dán token snc_pat_… vào stdin), \
             hoặc đặt biến môi trường SENCLAW_HUB_TOKEN. Tệp mong đợi: {}",
            path.display()
        )
    })?;
    let t = raw.trim().to_string();
    if t.is_empty() {
        bail!("{} rỗng — chạy lại `senclaw hub login`", path.display());
    }
    Ok(t)
}

/// Store a token with owner-only permissions.
pub fn write_token(token: &str) -> Result<std::path::PathBuf> {
    let t = token.trim();
    if !t.starts_with("snc_pat_") {
        bail!("token phải bắt đầu bằng `snc_pat_` (lấy ở trang hub → Settings → Tokens)");
    }
    let path = token_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, format!("{t}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// Versions already published for a package, if it exists at all.
///
/// A 404 means the name is free — that is not an error.
pub async fn published_versions(hub: &str, scope: &str, name: &str) -> Result<Vec<String>> {
    let url = format!(
        "{}/api/v1/packages/{scope}/{name}",
        hub.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let resp = client
        .get(&url)
        .header("accept", "application/vnd.senclaw.install-v1+json")
        .send()
        .await
        .with_context(|| format!("không gọi được {url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(vec![]);
    }
    if !resp.status().is_success() {
        bail!("{url} trả về HTTP {}", resp.status());
    }
    let doc: serde_json::Value = resp.json().await?;
    Ok(doc
        .get("versions")
        .and_then(|v| v.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default())
}

pub struct PublishRequest<'a> {
    pub hub: String,
    pub token: String,
    pub kind: &'a str,
    pub name: &'a str,
    pub version: &'a str,
    pub description: &'a str,
    pub keywords: &'a [String],
    pub category: Option<&'a str>,
    pub permissions: Option<&'a serde_json::Value>,
    pub repo_url: Option<&'a str>,
    pub homepage_url: Option<&'a str>,
    pub readme: Option<String>,
    pub platform: Option<&'a str>,
    pub extra: serde_json::Value,
    pub artifact: &'a Path,
}

#[derive(Debug, Deserialize)]
pub struct PublishOk {
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub integrity: String,
    #[serde(default)]
    pub url: String,
}

/// `sha512-<base64>`, the same shape the hub stores. Informational only.
pub fn sha512_integrity(bytes: &[u8]) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha512};
    let digest = Sha512::digest(bytes);
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// Upload one artifact. Every documented failure gets a message the caller can
/// act on, because "HTTP 409" alone does not tell anyone to bump the version.
pub async fn publish(req: PublishRequest<'_>) -> Result<PublishOk> {
    let bytes = std::fs::read(req.artifact)
        .with_context(|| format!("không đọc được {}", req.artifact.display()))?;
    if bytes.len() as u64 > MAX_UPLOAD_BYTES {
        bail!(
            "{} nặng {:.1} MB, vượt giới hạn {} MB của hub",
            req.artifact.display(),
            bytes.len() as f64 / 1_048_576.0,
            MAX_UPLOAD_BYTES / 1_048_576
        );
    }
    let filename = req
        .artifact
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("tên tệp không hợp lệ"))?
        .to_string();

    let mut form = reqwest::multipart::Form::new()
        .text("kind", req.kind.to_string())
        .text("name", req.name.to_string())
        .text("version", req.version.to_string())
        .text("description", req.description.to_string())
        .part(
            "file",
            reqwest::multipart::Part::bytes(bytes).file_name(filename),
        );

    if !req.keywords.is_empty() {
        form = form.text("keywords", req.keywords.join(","));
    }
    if let Some(c) = req.category {
        form = form.text("category", c.to_string());
    }
    if let Some(p) = req.permissions {
        form = form.text("permissions", p.to_string());
    }
    if let Some(u) = req.repo_url {
        form = form.text("repoUrl", u.to_string());
    }
    if let Some(u) = req.homepage_url {
        form = form.text("homepageUrl", u.to_string());
    }
    if let Some(r) = req.readme {
        form = form.text("readme", r);
    }
    if let Some(p) = req.platform {
        form = form.text("platform", p.to_string());
    }
    if !req.extra.is_null() {
        form = form.text("extra", req.extra.to_string());
    }

    let url = format!("{}/api/v1/publish", req.hub.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let resp = client
        .post(&url)
        .bearer_auth(&req.token)
        .multipart(form)
        .send()
        .await
        .with_context(|| format!("không gửi được tới {url}"))?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if status.is_success() {
        return Ok(serde_json::from_value(body)?);
    }

    let code = body
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let msg = body
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("(hub không kèm thông báo)");
    bail!("{}", explain(status.as_u16(), code, msg, req.version));
}

/// Turn a hub error into something the user can act on.
fn explain(status: u16, code: &str, msg: &str, version: &str) -> String {
    match (status, code) {
        (409, _) => format!(
            "phiên bản {version} đã tồn tại trên hub. Một name@version đã publish là BẤT BIẾN — \
             hãy tăng version (`senclaw hub bump <app> patch`) rồi publish lại.\n  hub: {msg}"
        ),
        (401, _) => format!(
            "token không hợp lệ hoặc đã hết hạn — chạy lại `senclaw hub login`.\n  hub: {msg}"
        ),
        (403, "insufficient_scope") => {
            format!("token thiếu quyền `publish` — tạo token mới có scope publish.\n  hub: {msg}")
        }
        (403, "no_handle") => {
            format!("tài khoản chưa chọn username trên hub — vào trang hub đặt username trước.\n  hub: {msg}")
        }
        (403, _) => format!("bạn không phải maintainer của gói này.\n  hub: {msg}"),
        (413, _) => format!("tệp vượt giới hạn kích thước của hub.\n  hub: {msg}"),
        _ => format!("publish thất bại (HTTP {status} · {code}): {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_accepts_only_three_numeric_parts() {
        for good in ["1.0.0", "0.1.0", "10.20.30", "1.0.0-beta.1", "1.2.3+build"] {
            assert!(is_semver(good), "should accept {good}");
        }
        // The two mistakes people actually make, plus leading zeros.
        for bad in ["1.0", "v1.0.0", "1.0.0.0", "1.a.0", "", "01.0.0"] {
            assert!(!is_semver(bad), "should reject {bad}");
        }
    }

    #[test]
    fn bump_moves_the_right_component_and_zeroes_the_rest() {
        assert_eq!(bump("1.2.3", "patch").unwrap(), "1.2.4");
        assert_eq!(bump("1.2.3", "minor").unwrap(), "1.3.0");
        assert_eq!(bump("1.2.3", "major").unwrap(), "2.0.0");
    }

    #[test]
    fn bump_refuses_a_non_semver_input_instead_of_guessing() {
        assert!(bump("1.0", "patch").is_err());
        assert!(bump("1.2.3", "build").is_err());
    }

    #[test]
    fn a_prerelease_bump_drops_the_prerelease_tag() {
        // 1.0.0-beta.1 → patch → 1.0.1, not 1.0.0-beta.2: the next *published*
        // version must sort above the prerelease.
        assert_eq!(bump("1.0.0-beta.1", "patch").unwrap(), "1.0.1");
    }

    #[test]
    fn host_platform_matches_the_hub_triple_style() {
        let p = host_platform();
        assert!(p.contains('-'), "{p}");
        assert!(!p.contains("macos"), "must be darwin-*, got {p}");
        assert!(!p.contains("aarch64"), "must be *-arm64, got {p}");
        // Rust's `windows` is the hub's `win32`; leaving it untranslated 404s
        // every Windows install against a correctly published package.
        assert!(!p.contains("windows"), "must be win32-*, got {p}");
        // Whatever this machine is, the hub must recognise the id verbatim.
        assert_eq!(canonical_platform(&p), p);
    }

    #[test]
    fn every_build_tool_spelling_canonicalises_to_the_hub_id() {
        for s in [
            "windows-x64",
            "win32-x64",
            "x86_64-pc-windows-msvc",
            "Windows x86_64",
            "win-amd64",
        ] {
            assert_eq!(canonical_platform(s), "win32-x64", "{s}");
        }
        assert_eq!(canonical_platform("darwin-aarch64"), "darwin-arm64");
        assert_eq!(canonical_platform("macos-arm64"), "darwin-arm64");
        assert_eq!(canonical_platform("aarch64-unknown-linux-gnu"), "linux-arm64");
        assert_eq!(canonical_platform("na"), "any");
        // Unrecognised input keeps its own spelling rather than collapsing to
        // something that would match the wrong artifact.
        assert_eq!(canonical_platform("Plan9-Sparc"), "plan9-sparc");
    }

    #[test]
    fn integrity_has_the_shape_the_hub_stores() {
        let s = sha512_integrity(b"hello");
        assert!(s.starts_with("sha512-"));
        // base64 of a 64-byte digest is 88 chars including padding.
        assert_eq!(s.len(), "sha512-".len() + 88);
    }

    #[test]
    fn a_version_conflict_tells_the_user_to_bump() {
        let e = explain(409, "version_exists", "already exists", "1.0.0");
        assert!(e.contains("BẤT BIẾN"));
        assert!(e.contains("bump"));
    }

    #[test]
    fn a_scope_error_is_distinguished_from_a_bad_token() {
        assert!(explain(401, "unauthorized", "x", "1.0.0").contains("login"));
        assert!(explain(403, "insufficient_scope", "x", "1.0.0").contains("publish"));
        assert!(explain(403, "no_handle", "x", "1.0.0").contains("username"));
    }

    #[test]
    fn a_hub_package_round_trips_without_losing_permissions() {
        let pkg = HubPackage {
            version: "1.0.1".into(),
            category: Some("productivity".into()),
            keywords: vec!["mcp".into()],
            permissions: Some(serde_json::json!({ "network": ["127.0.0.1"] })),
            homepage_url: None,
            repo_url: None,
            updater: "none".into(),
            platform: Some("darwin-arm64".into()),
            artifact: None,
        };
        let back: HubPackage = serde_json::from_str(&serde_json::to_string(&pkg).unwrap()).unwrap();
        assert_eq!(back.version, "1.0.1");
        assert_eq!(back.permissions.unwrap()["network"][0], "127.0.0.1");
    }
}

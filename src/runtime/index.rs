//! `runtimes/index.json` — the catalog of installable runtimes, fetched from
//! `SENCLAW_RUNTIME_INDEX_URL`, cached, with a bundled copy compiled into the
//! daemon as the offline fallback (`docs/runtime-protocol.md` §7.1).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::settings::Channel;

/// Compiled into the binary at build time — always available, even offline or
/// before the first successful fetch.
pub const BUNDLED_INDEX: &str = include_str!("../../runtimes/index.json");

pub const DEFAULT_INDEX_URL: &str =
    "https://raw.githubusercontent.com/SenClaw/senclaw/main/runtimes/index.json";

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageAsset {
    pub platform: String,
    pub url: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub notes_url: Option<String>,
    #[serde(default)]
    pub packages: Vec<PackageAsset>,
}

/// Upstream-build declaration (llama.cpp only) — resolved by
/// [`super::llamacpp`] rather than listing every release by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Upstream {
    pub kind: String,
    pub repo: String,
    pub binary: String,
    /// Platform key → asset name template (`{version}` substituted).
    pub assets: std::collections::BTreeMap<String, UpstreamAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamAsset {
    pub asset: String,
    /// Extra assets to extract alongside the main one, into the same
    /// directory (Windows CUDA's `cudart-*` archive).
    #[serde(default)]
    pub extra_assets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexEntry {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub runtime_type: sen_runtime_sdk::manifest::RuntimeType,
    #[serde(default)]
    pub description: String,
    pub slots: Vec<sen_runtime_sdk::manifest::Slot>,
    #[serde(default)]
    pub formats: Vec<sen_runtime_sdk::manifest::ModelFormat>,
    #[serde(default)]
    pub capabilities: Vec<sen_runtime_sdk::manifest::Capability>,
    #[serde(default)]
    pub accelerator: Option<String>,
    pub platforms: Vec<String>,
    /// Channel name → version that channel currently installs. `"latest"`
    /// (upstream only) means "resolve at install time", not a literal version.
    pub channels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub releases: Vec<Release>,
    #[serde(default)]
    pub upstream: Option<Upstream>,
}

impl IndexEntry {
    /// The version this channel currently points to, or `None` when the
    /// entry declares no such channel (an unpublished `sen-*` with
    /// `releases: []` still lists channels so the UI can show "coming soon").
    pub fn channel_version(&self, channel: Channel) -> Option<&str> {
        self.channels.get(channel.as_str()).map(String::as_str)
    }

    /// The release matching a resolved (non-`"latest"`) version.
    pub fn release(&self, version: &str) -> Option<&Release> {
        self.releases.iter().find(|r| r.version == version)
    }

    pub fn package_for<'a>(&self, release: &'a Release, platform: &str) -> Option<&'a PackageAsset> {
        release.packages.iter().find(|p| p.platform == platform)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeIndex {
    pub schema_version: u32,
    pub runtimes: Vec<IndexEntry>,
}

impl RuntimeIndex {
    pub fn parse(text: &str) -> Result<RuntimeIndex> {
        let index: RuntimeIndex = serde_json::from_str(text).context("parse runtimes index")?;
        if index.schema_version != SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported index schemaVersion {} (this build reads {SCHEMA_VERSION})",
                index.schema_version
            );
        }
        Ok(index)
    }

    pub fn bundled() -> RuntimeIndex {
        // The bundled copy ships with the binary and is validated by a unit
        // test below — this can only fail if a future edit breaks it.
        RuntimeIndex::parse(BUNDLED_INDEX).expect("bundled runtimes/index.json must parse")
    }

    pub fn entry(&self, id: &str) -> Option<&IndexEntry> {
        self.runtimes.iter().find(|e| e.id == id)
    }
}

/// A fetch result cached at `<runtimes_dir>/index-cache.json` so the daemon
/// has something to show (with an `error`) when offline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedIndex {
    pub fetched_at: u64,
    pub source: String,
    pub index: RuntimeIndex,
    /// `"latest"` (upstream only, the beta channel) resolved to a concrete
    /// `b<N>` tag, keyed by runtime id (§7.1). Resolving it costs a GitHub
    /// API call, so it is done once (by `RuntimeManager::refresh_index`, which
    /// also persists it back here) and read from the cache afterward, not on
    /// every `GET /api/runtimes/catalog`.
    #[serde(default)]
    pub resolved_latest: std::collections::BTreeMap<String, String>,
}

impl CachedIndex {
    /// `entry`'s version on `channel`, with `"latest"` swapped for the
    /// concrete tag once resolved — `None` for `"latest"` still unresolved,
    /// rather than reporting the literal string as if it were a real version
    /// (`docs/runtime-protocol.md` §7.3: "report `updateAvailable:false` until
    /// it is resolved").
    pub fn effective_channel_version(&self, entry: &IndexEntry, channel: Channel) -> Option<String> {
        let v = entry.channel_version(channel)?;
        if v == "latest" {
            self.resolved_latest.get(&entry.id).cloned()
        } else {
            Some(v.to_string())
        }
    }
}

fn cache_path(runtimes_dir: &Path) -> std::path::PathBuf {
    runtimes_dir.join("index-cache.json")
}

pub fn load_cache(runtimes_dir: &Path) -> Option<CachedIndex> {
    let raw = std::fs::read_to_string(cache_path(runtimes_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_cache(runtimes_dir: &Path, cached: &CachedIndex) -> Result<()> {
    std::fs::create_dir_all(runtimes_dir)?;
    let path = cache_path(runtimes_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(cached)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Record the concrete tag `"latest"` resolved to for `id`, so a later read of
/// the cache (no refresh) still reports it instead of leaving it unresolved.
/// A no-op when there is no cache yet — the next `refresh_index` will try
/// again and has a fresh `CachedIndex` to write the resolution onto anyway.
pub fn record_resolved_latest(runtimes_dir: &Path, id: &str, tag: &str) {
    let Some(mut cached) = load_cache(runtimes_dir) else { return };
    cached.resolved_latest.insert(id.to_string(), tag.to_string());
    if let Err(e) = save_cache(runtimes_dir, &cached) {
        tracing::warn!("[runtime] could not persist the resolved `latest` tag for {id}: {e:#}");
    }
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Fetch the index from `url`, cache it, and return it. On any failure, fall
/// back to the last cache, then the bundled copy — `error` on the returned
/// tuple names what went wrong, but a caller always gets something to show.
pub async fn fetch_and_cache(runtimes_dir: &Path, url: &str) -> (CachedIndex, Option<String>) {
    match fetch(url).await {
        Ok(index) => {
            // A fresh index may have a newer upstream release than whatever
            // `"latest"` last resolved to — start empty so the caller
            // (`RuntimeManager::refresh_index`) re-resolves it.
            let cached = CachedIndex { fetched_at: now_millis(), source: url.to_string(), index, resolved_latest: Default::default() };
            if let Err(e) = save_cache(runtimes_dir, &cached) {
                tracing::warn!("[runtime] could not cache the runtime index: {e:#}");
            }
            (cached, None)
        }
        Err(e) => {
            let error = e.to_string();
            tracing::warn!("[runtime] could not fetch {url}: {error}");
            let fallback = load_cache(runtimes_dir).unwrap_or_else(|| CachedIndex {
                fetched_at: 0,
                source: "bundled".to_string(),
                index: RuntimeIndex::bundled(),
                resolved_latest: Default::default(),
            });
            (fallback, Some(error))
        }
    }
}

async fn fetch(url: &str) -> Result<RuntimeIndex> {
    let text = if let Some(path) = url.strip_prefix("file://") {
        tokio::fs::read_to_string(path).await.with_context(|| format!("read {path}"))?
    } else {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        client
            .get(url)
            .send()
            .await
            .with_context(|| format!("fetch {url}"))?
            .error_for_status()
            .with_context(|| format!("fetch {url}"))?
            .text()
            .await
            .context("read response body")?
    };
    RuntimeIndex::parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_index_parses_and_covers_real_entries() {
        let index = RuntimeIndex::bundled();
        assert!(index.entry("llama.cpp-metal").is_some());
        assert!(index.entry("sen-ocr").is_some());
        let metal = index.entry("llama.cpp-metal").unwrap();
        assert_eq!(metal.channel_version(Channel::Stable), Some("b11201"));
        assert!(metal.upstream.is_some());
    }

    #[test]
    fn published_runtimes_list_real_packages_for_their_stable_version() {
        // The stable channel must name a release that is actually listed, and
        // every package must be that release's own asset with a usable
        // checksum — the installer verifies against it and refuses otherwise.
        let index = RuntimeIndex::bundled();
        for id in ["sen-mlx", "sen-sysone", "sen-ocr", "sen-whisper", "sen-tts"] {
            let entry = index.entry(id).unwrap_or_else(|| panic!("{id} missing from bundled index"));
            let stable = entry.channel_version(Channel::Stable).unwrap_or_else(|| panic!("{id} has no stable channel"));
            let release = entry
                .releases
                .iter()
                .find(|r| r.version == stable)
                .unwrap_or_else(|| panic!("{id}: stable names {stable}, which lists no release"));
            assert!(release.packages.iter().any(|p| p.platform == "darwin-arm64"), "{id}: no darwin-arm64 package");
            for p in &release.packages {
                let own = format!("https://github.com/SenClaw/{id}/releases/download/v{stable}/{id}-{stable}-{}.tar.gz", p.platform);
                assert_eq!(p.url, own, "{id}: package is not this release's own asset");
                let sha = p.sha256.as_deref().unwrap_or_default();
                assert!(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()), "{id} {}: bad sha256", p.platform);
                assert!(p.size.is_some_and(|s| s > 0), "{id} {}: no size", p.platform);
            }
        }
    }

    #[test]
    fn channel_resolution_reads_the_named_channel() {
        let text = r#"{"schemaVersion":1,"runtimes":[{"id":"x","name":"X","type":"ocr",
            "slots":["ocr"],"capabilities":["ocr"],"platforms":["darwin-arm64"],
            "channels":{"stable":"1.0.0","beta":"1.1.0-rc1"},"releases":[]}]}"#;
        let index = RuntimeIndex::parse(text).unwrap();
        let e = index.entry("x").unwrap();
        assert_eq!(e.channel_version(Channel::Stable), Some("1.0.0"));
        assert_eq!(e.channel_version(Channel::Beta), Some("1.1.0-rc1"));
    }

    #[test]
    fn effective_channel_version_passes_through_a_concrete_version_unchanged() {
        let text = r#"{"schemaVersion":1,"runtimes":[{"id":"x","name":"X","type":"ocr",
            "slots":["ocr"],"capabilities":["ocr"],"platforms":["darwin-arm64"],
            "channels":{"stable":"1.0.0"},"releases":[]}]}"#;
        let index = RuntimeIndex::parse(text).unwrap();
        let e = index.entry("x").unwrap();
        let cached = CachedIndex { fetched_at: 0, source: "test".into(), index: index.clone(), resolved_latest: Default::default() };
        assert_eq!(cached.effective_channel_version(e, Channel::Stable), Some("1.0.0".to_string()));
    }

    #[test]
    fn effective_channel_version_resolves_latest_from_the_cache_or_reports_unresolved() {
        let text = r#"{"schemaVersion":1,"runtimes":[{"id":"llama.cpp-metal","name":"X","type":"llm-engine",
            "slots":["gguf"],"capabilities":["chat"],"platforms":["darwin-arm64"],
            "channels":{"beta":"latest"},"releases":[],
            "upstream":{"kind":"llama.cpp","repo":"ggml-org/llama.cpp","binary":"llama-server","assets":{}}}]}"#;
        let index = RuntimeIndex::parse(text).unwrap();
        let e = index.entry("llama.cpp-metal").unwrap();

        let unresolved = CachedIndex { fetched_at: 0, source: "test".into(), index: index.clone(), resolved_latest: Default::default() };
        assert_eq!(
            unresolved.effective_channel_version(e, Channel::Beta),
            None,
            "must not report the literal string \"latest\" as a version"
        );

        let mut resolved_latest = std::collections::BTreeMap::new();
        resolved_latest.insert("llama.cpp-metal".to_string(), "b11300".to_string());
        let resolved = CachedIndex { fetched_at: 0, source: "test".into(), index: index.clone(), resolved_latest };
        assert_eq!(resolved.effective_channel_version(e, Channel::Beta), Some("b11300".to_string()));
    }

    #[test]
    fn a_newer_schema_is_refused_by_name() {
        let text = r#"{"schemaVersion":2,"runtimes":[]}"#;
        assert!(RuntimeIndex::parse(text).is_err());
    }

    #[test]
    fn file_url_fetch_reads_a_local_index() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.json");
        std::fs::write(&path, BUNDLED_INDEX).unwrap();
        let url = format!("file://{}", path.display());
        let index = tokio_test::block_on(fetch(&url)).unwrap();
        assert!(index.entry("llama.cpp-cpu").is_some());
    }

}

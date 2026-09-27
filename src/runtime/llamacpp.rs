//! Installing upstream llama.cpp as a runtime (`docs/runtime-protocol.md` §7.2).
//!
//! There is no `senclaw-runtime.json` upstream — a `llama-server` release is
//! just a build. The daemon downloads the platform asset from
//! `ggml-org/llama.cpp`'s GitHub releases, extracts it, finds `llama-server`
//! inside the tree, and **generates** the manifest before handing the result
//! to [`super::store::finish_install`].

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::index::{IndexEntry, Upstream, UpstreamAsset};
use super::store::{self, InstalledPackage};
use sen_runtime_sdk::manifest::{ApiDecl, Capability, Entry, Health, ModelFormat, RunMode, RuntimeManifest, RuntimeType, Slot};

const RELEASES_API: &str = "https://api.github.com/repos/ggml-org/llama.cpp/releases";

#[derive(Debug, Clone, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GhAsset>,
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("senclaw/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

/// A `b<digits>` release tag — the only kind that is a real build (the
/// GitHub "latest" release itself is not one).
fn is_build_tag(tag: &str) -> bool {
    tag.strip_prefix('b').is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// Pick the asset for `platform` out of an already-fetched release, given the
/// index's per-platform template (`{version}` substituted with the release's
/// own tag).
fn asset_for<'a>(release: &'a GhRelease, template: &UpstreamAsset) -> Option<&'a GhAsset> {
    let name = template.asset.replace("{version}", &release.tag_name);
    release.assets.iter().find(|a| a.name == name)
}

/// Newest `b<N>` release carrying an asset for `platform`, per the index's
/// template — this is what channel `"latest"` (beta) resolves to.
fn pick_latest<'a>(releases: &'a [GhRelease], template: &UpstreamAsset) -> Option<(&'a GhRelease, &'a GhAsset)> {
    releases
        .iter()
        .filter(|r| !r.prerelease || is_build_tag(&r.tag_name))
        .filter(|r| is_build_tag(&r.tag_name))
        .filter_map(|r| asset_for(r, template).map(|a| (r, a)))
        .max_by_key(|(r, _)| {
            r.tag_name.strip_prefix('b').and_then(|n| n.parse::<u64>().ok()).unwrap_or(0)
        })
}

/// `sha256:<hex>` (GitHub's asset digest format) → the hex string alone.
fn sha256_hex(digest: &Option<String>) -> Option<String> {
    digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).map(str::to_string)
}

async fn fetch_releases(client: &reqwest::Client) -> Result<Vec<GhRelease>> {
    client
        .get(RELEASES_API)
        .send()
        .await
        .context("fetch llama.cpp releases")?
        .error_for_status()
        .context("fetch llama.cpp releases")?
        .json()
        .await
        .context("parse llama.cpp releases")
}

async fn fetch_release_by_tag(client: &reqwest::Client, tag: &str) -> Result<GhRelease> {
    let url = format!("{RELEASES_API}/tags/{tag}");
    client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("fetch {url}"))?
        .json()
        .await
        .context("parse llama.cpp release")
}

/// One asset to download: url, expected sha256 (when GitHub supplied a
/// digest) and the file name to save it as.
struct ResolvedAsset {
    url: String,
    sha256: Option<String>,
    file_name: String,
}

struct Resolved {
    version: String,
    main: ResolvedAsset,
    extra: Vec<ResolvedAsset>,
}

/// Resolve `"latest"` to a concrete `b<N>` tag for `platform_key`, without
/// downloading anything — for reporting a real version in the catalog and
/// comparing it against what is installed (§7.1: "`\"latest\"` (upstream
/// only) = newest `b<digits>` release carrying the asset").
pub async fn resolve_latest_tag(upstream: &Upstream, platform_key: &str) -> Result<String> {
    let template = upstream
        .assets
        .get(platform_key)
        .ok_or_else(|| anyhow::anyhow!("no llama.cpp build published for platform `{platform_key}`"))?;
    let http = client()?;
    let releases = fetch_releases(&http).await?;
    pick_latest(&releases, template)
        .map(|(r, _)| r.tag_name.clone())
        .ok_or_else(|| anyhow::anyhow!("no llama.cpp build release carries an asset for `{platform_key}`"))
}

/// Resolve `requested_version` (a literal `b<N>` tag, or `"latest"`) to a
/// concrete release + its assets for `platform_key`.
async fn resolve(
    client: &reqwest::Client,
    upstream: &Upstream,
    platform_key: &str,
    requested_version: &str,
) -> Result<Resolved> {
    let template = upstream
        .assets
        .get(platform_key)
        .ok_or_else(|| anyhow::anyhow!("no llama.cpp build published for platform `{platform_key}`"))?;

    let (release, main_asset) = if requested_version == "latest" {
        let releases = fetch_releases(client).await?;
        pick_latest(&releases, template)
            .map(|(r, a)| (r.clone(), a.clone()))
            .ok_or_else(|| anyhow::anyhow!("no llama.cpp build release carries an asset for `{platform_key}`"))?
    } else {
        let release = fetch_release_by_tag(client, requested_version).await?;
        let asset = asset_for(&release, template)
            .ok_or_else(|| {
                anyhow::anyhow!("release `{requested_version}` has no asset for `{platform_key}`")
            })?
            .clone();
        (release, asset)
    };

    let extra = template
        .extra_assets
        .iter()
        .map(|name_template| {
            let name = name_template.replace("{version}", &release.tag_name);
            release
                .assets
                .iter()
                .find(|a| a.name == name)
                .map(|a| ResolvedAsset {
                    url: a.browser_download_url.clone(),
                    sha256: sha256_hex(&a.digest),
                    file_name: a.name.clone(),
                })
                .ok_or_else(|| anyhow::anyhow!("release `{}` has no extra asset `{name}`", release.tag_name))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Resolved {
        version: release.tag_name,
        main: ResolvedAsset {
            url: main_asset.browser_download_url,
            sha256: sha256_hex(&main_asset.digest),
            file_name: main_asset.name,
        },
        extra,
    })
}

/// `(received, total)` — `total` is `None` when the server sent no
/// `Content-Length`.
pub type ProgressFn<'a> = &'a (dyn Fn(u64, Option<u64>) + Send + Sync);

async fn download_to(
    client: &reqwest::Client,
    asset: &ResolvedAsset,
    dest: &Path,
    on_progress: ProgressFn<'_>,
    cancel: &CancellationToken,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let resp = client
        .get(&asset.url)
        .send()
        .await
        .with_context(|| format!("download {}", asset.url))?
        .error_for_status()
        .with_context(|| format!("download {}", asset.url))?;
    let total = resp.content_length();
    let mut resp = resp;
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mut file = tokio::fs::File::create(dest).await.with_context(|| format!("create {}", dest.display()))?;
    let mut received: u64 = 0;
    while let Some(chunk) = resp.chunk().await? {
        if cancel.is_cancelled() {
            drop(file);
            let _ = tokio::fs::remove_file(dest).await;
            bail!("cancelled");
        }
        sha2::Digest::update(&mut hasher, &chunk);
        file.write_all(&chunk).await?;
        received += chunk.len() as u64;
        on_progress(received, total);
    }
    file.flush().await?;
    if let Some(expected) = &asset.sha256 {
        let got = hex::encode(sha2::Digest::finalize(hasher));
        if !got.eq_ignore_ascii_case(expected) {
            bail!("sha256 mismatch for {}: expected {expected}, got {got}", asset.file_name);
        }
    }
    Ok(())
}

/// Find `binary_name` (case-sensitive stem; `.exe` matched too) anywhere in
/// the extracted tree, returning its path relative to `root`.
fn find_binary(root: &Path, binary_name: &str) -> Result<PathBuf> {
    fn walk(dir: &Path, binary_name: &str, out: &mut Option<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                walk(&path, binary_name, out)?;
            } else {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == binary_name || name == format!("{binary_name}.exe") {
                    *out = Some(path);
                }
            }
        }
        Ok(())
    }
    let mut found = None;
    walk(root, binary_name, &mut found)?;
    found
        .and_then(|p| p.strip_prefix(root).ok().map(Path::to_path_buf))
        .ok_or_else(|| anyhow::anyhow!("`{binary_name}` not found in the extracted archive"))
}

/// The index entry's identity, carried into the generated manifest.
struct EntryLabel<'a> {
    id: &'a str,
    name: &'a str,
    description: &'a str,
    accelerator: Option<&'a str>,
}

/// Generate the manifest §7.2 describes for a resolved llama.cpp build.
///
/// Name, description and accelerator come from the index entry: the Runtime
/// screen shows the name beside a version chip, so a name with the version
/// baked in ("llama.cpp b11201") printed it twice and lost which variant
/// (Metal, CPU, Vulkan, CUDA) was installed.
fn generate_manifest(label: &EntryLabel<'_>, version: &str, platform_key: &str, command: &Path) -> RuntimeManifest {
    let command = command.to_string_lossy().replace('\\', "/");
    RuntimeManifest {
        schema_version: sen_runtime_sdk::manifest::SCHEMA_VERSION,
        id: label.id.to_string(),
        name: label.name.to_string(),
        version: version.to_string(),
        description: if label.description.trim().is_empty() {
            format!("Upstream llama.cpp ({platform_key})")
        } else {
            label.description.to_string()
        },
        runtime_type: RuntimeType::LlmEngine,
        slots: vec![Slot::Gguf],
        formats: vec![ModelFormat::Gguf],
        capabilities: vec![Capability::Chat, Capability::Embedding, Capability::Vision],
        platforms: vec![platform_key.to_string()],
        accelerator: label.accelerator.map(str::to_string),
        mode: RunMode::Model,
        entry: Entry {
            command,
            args: vec![
                "-m".into(),
                "{model_path}".into(),
                "--host".into(),
                "{host}".into(),
                "--port".into(),
                "{port}".into(),
                "--api-key".into(),
                "{token}".into(),
                "-c".into(),
                "{context_length}".into(),
            ],
            env: Default::default(),
            capability_args: [
                (Capability::Embedding, vec!["--embedding".to_string()]),
                (Capability::Vision, vec!["--mmproj".to_string(), "{mmproj_path}".to_string()]),
            ]
            .into_iter()
            .collect(),
        },
        health: Health { path: "/health".to_string(), startup_timeout_secs: 600 },
        idle_timeout_secs: None,
        api: ApiDecl::default(),
        homepage: Some("https://github.com/ggml-org/llama.cpp".to_string()),
        release_notes_url: Some(format!("https://github.com/ggml-org/llama.cpp/releases/tag/{version}")),
        license: Some("MIT".to_string()),
    }
}

/// Install `id` (one of the `llama.cpp-*` index entries) at `requested_version`
/// (a literal tag or `"latest"`) for the current platform. `cancel` is
/// checked between chunks of every download, same as the direct-release
/// install path — a cancelled llama.cpp install used to keep downloading to
/// completion regardless of the token.
pub async fn install(
    runtimes_dir: &Path,
    entry: &IndexEntry,
    requested_version: &str,
    on_progress: ProgressFn<'_>,
    cancel: &CancellationToken,
) -> Result<InstalledPackage> {
    let upstream = entry
        .upstream
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("`{}` has no upstream declaration in the index", entry.id))?;
    let platform_key = sen_runtime_sdk::platform::current();
    let http = client()?;
    let resolved = resolve(&http, upstream, platform_key, requested_version).await?;

    let staged_parent = store::scratch_dir(runtimes_dir)?;
    let downloads = staged_parent.join("downloads");
    std::fs::create_dir_all(&downloads)?;
    let extracted = staged_parent.join("pkg");
    std::fs::create_dir_all(&extracted)?;

    let result = install_inner(&http, &resolved, &downloads, &extracted, on_progress, cancel).await;
    match result {
        Ok(()) => {}
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staged_parent);
            return Err(e);
        }
    }

    let label = EntryLabel {
        id: &entry.id,
        name: &entry.name,
        description: &entry.description,
        accelerator: entry.accelerator.as_deref(),
    };
    let manifest = generate_manifest(
        &label,
        &resolved.version,
        platform_key,
        &find_binary(&extracted, &upstream.binary)?,
    );
    if let Err(e) = std::fs::write(extracted.join(sen_runtime_sdk::manifest::MANIFEST_FILE), serde_json::to_string_pretty(&manifest)?) {
        let _ = std::fs::remove_dir_all(&staged_parent);
        return Err(e.into());
    }

    let outcome = store::finish_install(runtimes_dir, extracted, store::PackageSource::Index);
    let _ = std::fs::remove_dir_all(&staged_parent);
    outcome
}

async fn install_inner(
    http: &reqwest::Client,
    resolved: &Resolved,
    downloads: &Path,
    extracted: &Path,
    on_progress: ProgressFn<'_>,
    cancel: &CancellationToken,
) -> Result<()> {
    let main_path = downloads.join(&resolved.main.file_name);
    download_to(http, &resolved.main, &main_path, on_progress, cancel).await?;
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    extract_archive(&main_path, extracted)?;
    for asset in &resolved.extra {
        let path = downloads.join(&asset.file_name);
        download_to(http, asset, &path, on_progress, cancel).await?;
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        extract_archive(&path, extracted)?;
    }
    Ok(())
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let name = archive.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        store::extract_tar_gz(archive, dest)
    } else if name.ends_with(".zip") {
        store::extract_zip(archive, dest)
    } else {
        bail!("`{name}` is neither a .tar.gz nor a .zip archive")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str, digest: Option<&str>) -> GhAsset {
        GhAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example.invalid/{name}"),
            digest: digest.map(str::to_string),
        }
    }

    fn release(tag: &str, prerelease: bool, assets: Vec<GhAsset>) -> GhRelease {
        GhRelease { tag_name: tag.to_string(), prerelease, assets }
    }

    #[test]
    fn only_b_digit_tags_are_builds() {
        assert!(is_build_tag("b11201"));
        assert!(!is_build_tag("latest"));
        assert!(!is_build_tag("b"));
        assert!(!is_build_tag("beta1"));
        assert!(!is_build_tag("v0.1.0"));
    }

    #[test]
    fn sha256_hex_strips_the_algorithm_prefix() {
        assert_eq!(sha256_hex(&Some("sha256:abc123".into())).as_deref(), Some("abc123"));
        assert_eq!(sha256_hex(&None), None);
        assert_eq!(sha256_hex(&Some("md5:xyz".into())), None);
    }

    #[test]
    fn pick_latest_finds_the_highest_build_number_with_the_asset() {
        let template = UpstreamAsset { asset: "llama-{version}-bin-macos-arm64.tar.gz".into(), extra_assets: vec![] };
        let releases = vec![
            release("b11100", false, vec![asset("llama-b11100-bin-macos-arm64.tar.gz", None)]),
            release("b11201", false, vec![asset("llama-b11201-bin-macos-arm64.tar.gz", None)]),
            // A newer tag with no matching asset must not win.
            release("b11300", false, vec![asset("llama-b11300-bin-linux-x64.tar.gz", None)]),
        ];
        let (r, a) = pick_latest(&releases, &template).unwrap();
        assert_eq!(r.tag_name, "b11201");
        assert_eq!(a.name, "llama-b11201-bin-macos-arm64.tar.gz");
    }

    #[test]
    fn asset_for_substitutes_the_version_placeholder() {
        let template = UpstreamAsset { asset: "llama-{version}-bin-win-cpu-x64.zip".into(), extra_assets: vec![] };
        let r = release("b11201", false, vec![asset("llama-b11201-bin-win-cpu-x64.zip", Some("sha256:deadbeef"))]);
        let found = asset_for(&r, &template).unwrap();
        assert_eq!(found.name, "llama-b11201-bin-win-cpu-x64.zip");
    }

    #[test]
    fn find_binary_locates_the_server_inside_a_nested_extraction() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("llama-b11201").join("bin");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("llama-server"), "binary").unwrap();
        let rel = find_binary(tmp.path(), "llama-server").unwrap();
        assert_eq!(rel, PathBuf::from("llama-b11201/bin/llama-server"));
    }

    #[test]
    fn find_binary_matches_the_windows_exe_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("llama-server.exe"), "binary").unwrap();
        let rel = find_binary(tmp.path(), "llama-server").unwrap();
        assert_eq!(rel, PathBuf::from("llama-server.exe"));
    }

    #[test]
    fn generated_manifest_matches_the_documented_launch_shape() {
        let label = EntryLabel { id: "llama.cpp-metal", name: "Metal llama.cpp", description: "", accelerator: Some("metal") };
        let m = generate_manifest(&label, "b11201", "darwin-arm64", Path::new("llama-b11201/llama-server"));
        assert_eq!(m.name, "Metal llama.cpp", "the index name, never one with the version baked in");
        assert_eq!(m.accelerator.as_deref(), Some("metal"));
        assert_eq!(m.description, "Upstream llama.cpp (darwin-arm64)");
        assert_eq!(m.mode, RunMode::Model);
        assert_eq!(m.entry.command, "llama-b11201/llama-server");
        assert_eq!(
            m.entry.args,
            vec!["-m", "{model_path}", "--host", "{host}", "--port", "{port}", "--api-key", "{token}", "-c", "{context_length}"]
        );
        assert_eq!(m.entry.capability_args[&Capability::Embedding], vec!["--embedding"]);
        assert_eq!(m.entry.capability_args[&Capability::Vision], vec!["--mmproj", "{mmproj_path}"]);
        assert_eq!(m.health.startup_timeout_secs, 600);
        m.validate().expect("generated manifest must be valid per the SDK's own rules");
    }
}

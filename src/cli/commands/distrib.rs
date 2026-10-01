//! `senclaw web`, `senclaw update` — the daemon's own binary and Web UI
//! bundle, downloaded from GitHub Releases on demand.
//!
//! The desktop app bundle is a release of the `desktop` repo: installing it
//! from the CLI is `senclaw install desktop` (see `desktop.rs`, which shares
//! the download helpers below), and keeping it current is the app's own
//! `update_desktop` binary. A CLI-installed daemon has no media sidecar to
//! fetch either (speech-to-text is the `sen-whisper` runtime now — `senclaw
//! runtime install sen-whisper`).
//!
//! Release asset names must match this repo's own release workflow
//! (`.github/workflows/release.yml`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use futures::StreamExt;

/// The daemon binary + release archives: `SenClaw/senclaw`.
const REPO: &str = "SenClaw/senclaw";
/// The Web UI bundle's own repo and release asset.
const WEB_APP_REPO: &str = "SenClaw/web-app";
const WEB_DIST_ASSET: &str = "senclaw-web-dist.tar.gz";

/// `senclaw web` — make sure the Web UI bundle exists locally, then start the
/// daemon with `SENCLAW_WEB_DIST` pointing at it.
pub async fn run_web(force: bool, version: Option<String>) -> Result<()> {
    let dist = ensure_web_dist(force, version).await?;
    println!("Serving Web UI from {}", dist.display());
    std::env::set_var("SENCLAW_WEB_DIST", &dist);

    let mut cfg = crate::config::Config::from_env();
    let gcp = cfg.paths.global_config_path.clone();
    cfg.apply_persisted_overrides(&gcp);
    let port = cfg.ui_server.port;
    println!("Web UI: http://127.0.0.1:{port}");
    crate::run_daemon(cfg).await
}

// ===== Update =====

/// `senclaw update` — update the binary, and the Web UI bundle if one was
/// previously downloaded.
pub async fn run_update(version: Option<String>) -> Result<()> {
    println!("Updating SenClaw…");
    update_binary(version.as_deref()).await?;

    let web_dist = home().join(".senclaw").join("web").join("dist");
    if web_dist.join("index.html").exists() {
        println!("\nUpdating Web UI…");
        ensure_web_dist(true, version).await?;
    }

    println!("\nAll components updated successfully.");
    Ok(())
}

/// Download the latest senclaw binary and replace the current one.
async fn update_binary(version: Option<&str>) -> Result<()> {
    let target = binary_target()?;
    let asset = format!("senclaw-{target}{}", std::env::consts::EXE_SUFFIX);
    let url = asset_url(REPO, &asset, version);
    let tmp = tmp_dir()?;
    let tmp_bin = tmp.join("senclaw-update");

    download(&url, &tmp_bin).await?;
    make_executable(&tmp_bin)?;

    let current_exe = std::env::current_exe().context("cannot determine current binary path")?;
    let current_exe = current_exe.canonicalize().unwrap_or_else(|_| current_exe.clone());

    // On Unix we can atomically rename over the running binary.
    // On Windows the running exe is locked, so we rename-away first.
    #[cfg(windows)]
    {
        let bak = current_exe.with_extension("exe.bak");
        let _ = std::fs::remove_file(&bak);
        std::fs::rename(&current_exe, &bak)
            .context("cannot move current binary aside — try running from an elevated prompt")?;
    }

    std::fs::rename(&tmp_bin, &current_exe).with_context(|| {
        format!(
            "cannot replace {} — you may need to run with sudo or adjust permissions",
            current_exe.display()
        )
    })?;

    println!("Binary updated: {}", current_exe.display());
    if let Ok(out) = std::process::Command::new(&current_exe).arg("--version").output() {
        print!("{}", String::from_utf8_lossy(&out.stdout));
    }
    Ok(())
}

/// Rust target triple matching this repo's release asset names.
pub(super) fn binary_target() -> Result<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Ok("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Ok("x86_64-pc-windows-msvc")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Ok("x86_64-unknown-linux-gnu")
    } else {
        bail!("no prebuilt binary for this platform — build from source: `cargo build --release`")
    }
}

// ===== Web UI bundle =====

/// Return the local Web UI dist directory, downloading and extracting the
/// release bundle on first use (or when `force` is set).
async fn ensure_web_dist(force: bool, version: Option<String>) -> Result<PathBuf> {
    let dist = home().join(".senclaw").join("web").join("dist");
    if !force && dist.join("index.html").exists() {
        return Ok(dist);
    }

    let tmp = tmp_dir()?;
    let tar_path = tmp.join(WEB_DIST_ASSET);
    download(&asset_url(WEB_APP_REPO, WEB_DIST_ASSET, version.as_deref()), &tar_path).await?;

    let _ = std::fs::remove_dir_all(&dist);
    std::fs::create_dir_all(&dist)?;
    // `tar` ships with macOS, Linux, and Windows 10 1803+.
    run_tool("tar", &["-xzf", &tar_path.to_string_lossy(), "-C", &dist.to_string_lossy()])?;
    let _ = std::fs::remove_file(&tar_path);

    if !dist.join("index.html").exists() {
        bail!(
            "extracted bundle has no index.html at {} — the release asset may be malformed",
            dist.display()
        );
    }
    println!("Web UI bundle installed at {}", dist.display());
    Ok(dist)
}

// ===== Shared helpers =====

/// Give `path` the executable bit. No-op on Windows, where the extension decides.
fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("cannot mark {} executable", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(super) fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub(super) fn tmp_dir() -> Result<PathBuf> {
    let dir = home().join(".senclaw").join("tmp");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub(super) fn asset_url(repo: &str, asset: &str, version: Option<&str>) -> String {
    match version {
        Some(tag) => {
            let tag = if tag.starts_with('v') { tag.to_string() } else { format!("v{tag}") };
            format!("https://github.com/{repo}/releases/download/{tag}/{asset}")
        }
        None => format!("https://github.com/{repo}/releases/latest/download/{asset}"),
    }
}

pub(super) async fn download(url: &str, dest: &Path) -> Result<()> {
    println!("Downloading {url}");
    let client = reqwest::Client::builder().user_agent(format!("senclaw/{}", env!("CARGO_PKG_VERSION"))).build()?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        bail!("download failed with HTTP {} — {url} (no matching release asset?)", resp.status());
    }
    let total = resp.content_length();

    let mut file = tokio::fs::File::create(dest).await.with_context(|| format!("create {}", dest.display()))?;
    let mut stream = resp.bytes_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        written += chunk.len() as u64;
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;

    match total {
        Some(t) => println!("Downloaded {} MB", t / 1_048_576),
        None => println!("Downloaded {} MB", written / 1_048_576),
    }
    Ok(())
}

pub(super) fn run_tool(program: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run `{program}` — is it installed?"))?;
    if !status.success() {
        bail!("`{program} {}` exited with {status}", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_url_defaults_to_latest_and_normalises_the_version_tag() {
        assert_eq!(
            asset_url(REPO, "senclaw-x86_64-unknown-linux-gnu", None),
            "https://github.com/SenClaw/senclaw/releases/latest/download/senclaw-x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            asset_url(WEB_APP_REPO, WEB_DIST_ASSET, Some("0.3.0")),
            "https://github.com/SenClaw/web-app/releases/download/v0.3.0/senclaw-web-dist.tar.gz"
        );
        assert_eq!(
            asset_url(REPO, "x", Some("v0.3.0")),
            "https://github.com/SenClaw/senclaw/releases/download/v0.3.0/x",
            "a `v`-prefixed tag is not double-prefixed"
        );
    }

    #[test]
    fn binary_target_is_known_on_this_platform_or_names_the_fallback() {
        match binary_target() {
            Ok(t) => assert!(!t.is_empty()),
            Err(e) => assert!(e.to_string().contains("build from source")),
        }
    }
}

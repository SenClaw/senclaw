//! `senclaw update` — list the daemon's GitHub releases, pick one (the latest
//! by default, a named version, or interactively), verify it and swap it in
//! for the running binary. The Web UI bundle, when one was downloaded before,
//! follows the web-app repo's latest release: its versions are its own and do
//! not track the daemon's.
//!
//! A daemon inside the desktop app's bundle is never replaced here: the app
//! ships daemon and UI as one release, and its own updater swaps the whole
//! bundle. Overwriting only the daemon would leave the app carrying two
//! versions from two releases.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use semver::Version;
use serde::Deserialize;

use super::distrib::{binary_target, download, ensure_web_dist, home, make_executable, tmp_dir, REPO};

/// How many releases `--list` and `--select` show.
const SHOWN_RELEASES: usize = 15;

/// What `senclaw update` was asked to do.
#[derive(Debug, Default, Clone)]
pub struct UpdateOptions {
    /// Version to install (`0.1.1` or `v0.1.1`); `None` means the latest.
    pub version: Option<String>,
    /// Print the releases and stop.
    pub list: bool,
    /// Choose the version from a numbered list.
    pub select: bool,
    /// Consider prereleases for "latest", `--list` and `--select`.
    pub pre: bool,
    /// Go ahead with a downgrade without asking.
    pub yes: bool,
    /// Reinstall even when the chosen version is the one running.
    pub force: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    /// `sha256:<hex>`, published by GitHub for every uploaded asset.
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

/// One installable daemon release, as far as this platform is concerned.
#[derive(Debug, Clone)]
struct Candidate {
    tag: String,
    version: Version,
    prerelease: bool,
    /// `YYYY-MM-DD`, or empty when GitHub gave none.
    date: String,
    /// This platform's binary, when the release built one.
    asset: Option<GhAsset>,
}

/// Where the chosen version sits relative to the running one.
#[derive(Debug, PartialEq, Eq)]
enum Direction {
    Same,
    Upgrade,
    Downgrade,
}

pub async fn run(opts: UpdateOptions) -> Result<()> {
    let current = current_version();
    let asset_name = binary_asset_name()?;
    let releases = fetch_releases().await?;
    // A version asked for by name is found even when it is a prerelease.
    let candidates = candidates(&releases, &asset_name, opts.pre || opts.version.is_some());

    if opts.list {
        print_list(&candidates, &current);
        return Ok(());
    }

    let chosen = if opts.select {
        select_interactively(&candidates, &current)?
    } else {
        resolve(&candidates, opts.version.as_deref())?
    };
    let Some(asset) = chosen.asset.clone() else {
        bail!("{} has no prebuilt binary for this platform ({asset_name}) — pick another version", chosen.tag);
    };

    // Only a version the person named or picked may go backwards; "latest"
    // older than this build (a dev build, say) means there is nothing newer.
    let explicit = opts.select || opts.version.is_some();
    match direction(&current, &chosen.version) {
        Direction::Downgrade if !explicit => {
            println!(
                "SenClaw {current} is newer than the latest release ({}). Nothing to update; \
                 name a version to go back to it.",
                chosen.tag
            );
            return Ok(());
        }
        Direction::Same if !opts.force => {
            println!("SenClaw {} is already installed. Pass --force to reinstall it.", chosen.tag);
            return Ok(());
        }
        Direction::Downgrade if !opts.yes => {
            println!("This replaces SenClaw {current} with the older {}.", chosen.tag);
            println!("Data written by the newer version may not read back in an older one.");
            if !confirm("Downgrade?")? {
                println!("Nothing changed.");
                return Ok(());
            }
        }
        _ => {}
    }

    let current_exe = current_exe()?;
    if let Some(bundle) = desktop_bundle_of(&current_exe) {
        bail!(
            "this senclaw is part of the desktop app at {} — update the app instead (its own updater, or \
             `senclaw install desktop --version {}`), so daemon and app stay one release",
            bundle.display(),
            chosen.tag
        );
    }

    println!("Updating SenClaw {current} → {}…", chosen.tag);
    install_binary(&asset, &current_exe).await?;

    let web_dist = home().join(".senclaw").join("web").join("dist");
    if web_dist.join("index.html").exists() {
        println!("\nUpdating Web UI…");
        ensure_web_dist(true, None).await?;
    }

    println!("\nSenClaw {} installed. Restart a running daemon to use it.", chosen.tag);
    Ok(())
}

// ===== Releases =====

fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("Cargo.toml carries a semver version")
}

fn binary_asset_name() -> Result<String> {
    Ok(format!("senclaw-{}{}", binary_target()?, std::env::consts::EXE_SUFFIX))
}

/// `v0.1.1` / `0.1.1` → the version; anything else (`nightly`, …) is not a
/// daemon release.
fn parse_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

async fn fetch_releases() -> Result<Vec<GhRelease>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100");
    let client = reqwest::Client::builder()
        .user_agent(format!("senclaw/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("cannot reach GitHub to list SenClaw releases")?;
    if resp.status() == reqwest::StatusCode::FORBIDDEN || resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        bail!("GitHub refused the release list (HTTP {}) — its unauthenticated rate limit is 60 requests an hour; try again later", resp.status());
    }
    resp.error_for_status()
        .with_context(|| format!("list releases at {url}"))?
        .json()
        .await
        .context("parse the release list")
}

/// Published daemon releases, newest version first. Prereleases are kept
/// only with `include_pre`; drafts and tags that are not versions never are.
fn candidates(releases: &[GhRelease], asset_name: &str, include_pre: bool) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = releases
        .iter()
        .filter(|r| !r.draft)
        .filter_map(|r| {
            let version = parse_tag(&r.tag_name)?;
            let prerelease = r.prerelease || !version.pre.is_empty();
            Some(Candidate {
                tag: r.tag_name.clone(),
                version,
                prerelease,
                date: r.published_at.as_deref().map(|d| d.chars().take(10).collect()).unwrap_or_default(),
                asset: r.assets.iter().find(|a| a.name == asset_name).cloned(),
            })
        })
        .filter(|c| include_pre || !c.prerelease)
        .collect();
    out.sort_by(|a, b| b.version.cmp(&a.version));
    out
}

/// The release to install: the named one, or the newest with a binary for
/// this platform.
fn resolve<'a>(candidates: &'a [Candidate], requested: Option<&str>) -> Result<&'a Candidate> {
    match requested {
        Some(req) => {
            let Some(want) = parse_tag(req.trim()) else {
                bail!("`{req}` is not a version — expected something like 0.1.1 or v0.1.1");
            };
            candidates.iter().find(|c| c.version == want).ok_or_else(|| {
                anyhow::anyhow!(
                    "no SenClaw release {want}. Available: {}",
                    available_list(candidates)
                )
            })
        }
        None => candidates
            .iter()
            .find(|c| c.asset.is_some())
            .ok_or_else(|| anyhow::anyhow!("no SenClaw release has a prebuilt binary for this platform yet")),
    }
}

fn available_list(candidates: &[Candidate]) -> String {
    let tags: Vec<&str> = candidates.iter().take(SHOWN_RELEASES).map(|c| c.tag.as_str()).collect();
    if tags.is_empty() {
        "none".into()
    } else {
        tags.join(", ")
    }
}

fn direction(current: &Version, target: &Version) -> Direction {
    match target.cmp(current) {
        std::cmp::Ordering::Equal => Direction::Same,
        std::cmp::Ordering::Greater => Direction::Upgrade,
        std::cmp::Ordering::Less => Direction::Downgrade,
    }
}

/// One line of `--list` / `--select`: tag, date and what is notable about it.
fn describe(c: &Candidate, current: &Version, latest: Option<&Version>) -> String {
    let mut notes = Vec::new();
    if &c.version == current {
        notes.push("installed");
    }
    if latest == Some(&c.version) {
        notes.push("latest");
    }
    if c.prerelease {
        notes.push("prerelease");
    }
    if c.asset.is_none() {
        notes.push("no build for this platform");
    }
    let notes = if notes.is_empty() { String::new() } else { format!("  ({})", notes.join(", ")) };
    format!("{:<12} {:<10}{notes}", c.tag, c.date)
}

fn latest_installable(candidates: &[Candidate]) -> Option<&Version> {
    candidates.iter().find(|c| c.asset.is_some()).map(|c| &c.version)
}

fn print_list(candidates: &[Candidate], current: &Version) {
    println!("SenClaw {current} is running. Releases:");
    if candidates.is_empty() {
        println!("  (none)");
        return;
    }
    let latest = latest_installable(candidates);
    for c in candidates.iter().take(SHOWN_RELEASES) {
        println!("  {}", describe(c, current, latest));
    }
    println!("\nInstall one with: senclaw update <version>");
}

/// Number the installable releases and read a choice: a list number, a
/// version, or Enter for the latest.
fn select_interactively<'a>(candidates: &'a [Candidate], current: &Version) -> Result<&'a Candidate> {
    if !std::io::stdin().is_terminal() {
        bail!("--select needs a terminal; name the version instead: senclaw update <version>");
    }
    let installable: Vec<&Candidate> = candidates.iter().filter(|c| c.asset.is_some()).take(SHOWN_RELEASES).collect();
    if installable.is_empty() {
        bail!("no SenClaw release has a prebuilt binary for this platform yet");
    }
    let latest = latest_installable(candidates);
    println!("SenClaw {current} is running. Choose a version:");
    for (i, c) in installable.iter().enumerate() {
        println!("  {:>2}) {}", i + 1, describe(c, current, latest));
    }
    loop {
        print!("Number or version [1]: ");
        std::io::stdout().flush().ok();
        let Some(line) = read_line()? else { bail!("no version chosen") };
        match pick(&installable, &line) {
            Some(c) => return Ok(c),
            None => println!("`{}` is not one of the listed versions.", line.trim()),
        }
    }
}

/// Map a `--select` answer onto the numbered list.
fn pick<'a>(installable: &[&'a Candidate], answer: &str) -> Option<&'a Candidate> {
    let answer = answer.trim();
    if answer.is_empty() {
        return installable.first().copied();
    }
    if let Ok(n) = answer.parse::<usize>() {
        if (1..=installable.len()).contains(&n) {
            return Some(installable[n - 1]);
        }
    }
    let want = parse_tag(answer)?;
    installable.iter().copied().find(|c| c.version == want)
}

fn read_line() -> Result<Option<String>> {
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line).context("read the answer")?;
    Ok((n > 0).then_some(line))
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("{question} Refusing without a terminal to ask on — pass --yes to go ahead");
    }
    print!("{question} [y/N]: ");
    std::io::stdout().flush().ok();
    Ok(matches!(read_line()?.as_deref().map(str::trim), Some("y" | "Y" | "yes" | "Yes")))
}

// ===== Install =====

fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot determine current binary path")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// The desktop app bundle `exe` belongs to, if any: a macOS `.app` around it,
/// or (Windows/Linux) the app's own executable beside it.
fn desktop_bundle_of(exe: &Path) -> Option<PathBuf> {
    if let Some(app) = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")) {
        return Some(app.to_path_buf());
    }
    let dir = exe.parent()?;
    ["senclaw_desktop", "senclaw_desktop.exe"]
        .iter()
        .any(|name| dir.join(name).is_file())
        .then(|| dir.to_path_buf())
}

/// `sha256:<hex>` → `<hex>`.
fn sha256_hex(digest: Option<&str>) -> Option<&str> {
    digest.and_then(|d| d.strip_prefix("sha256:"))
}

fn file_sha256(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).context("read the downloaded binary")?;
    Ok(hex::encode(hasher.finalize()))
}

/// Download `asset`, check it against GitHub's digest, and put it in place of
/// `current_exe`.
async fn install_binary(asset: &GhAsset, current_exe: &Path) -> Result<()> {
    let tmp_bin = tmp_dir()?.join("senclaw-update");
    download(&asset.browser_download_url, &tmp_bin).await?;

    match sha256_hex(asset.digest.as_deref()) {
        Some(expected) => {
            let actual = file_sha256(&tmp_bin)?;
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = std::fs::remove_file(&tmp_bin);
                bail!("checksum mismatch for the downloaded binary — refusing to install\n  expected {expected}\n  actual   {actual}");
            }
            println!("Checksum verified.");
        }
        None => println!("GitHub published no checksum for this asset; installing unverified."),
    }
    make_executable(&tmp_bin)?;

    // On Unix we can atomically rename over the running binary.
    // On Windows the running exe is locked, so we rename-away first.
    #[cfg(windows)]
    {
        let bak = current_exe.with_extension("exe.bak");
        let _ = std::fs::remove_file(&bak);
        std::fs::rename(current_exe, &bak)
            .context("cannot move current binary aside — try running from an elevated prompt")?;
    }

    std::fs::rename(&tmp_bin, current_exe).with_context(|| {
        format!(
            "cannot replace {} — you may need to run with sudo or adjust permissions",
            current_exe.display()
        )
    })?;

    println!("Binary updated: {}", current_exe.display());
    if let Ok(out) = std::process::Command::new(current_exe).arg("--version").output() {
        print!("{}", String::from_utf8_lossy(&out.stdout));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASSET: &str = "senclaw-aarch64-apple-darwin";

    fn release(tag: &str, prerelease: bool, with_asset: bool) -> GhRelease {
        GhRelease {
            tag_name: tag.into(),
            prerelease,
            draft: false,
            published_at: Some("2026-10-01T05:00:00Z".into()),
            assets: if with_asset {
                vec![GhAsset {
                    name: ASSET.into(),
                    browser_download_url: format!("https://example.invalid/{tag}/{ASSET}"),
                    digest: Some("sha256:abc".into()),
                }]
            } else {
                vec![]
            },
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn candidates_are_newest_first_and_skip_drafts_prereleases_and_non_versions() {
        let mut draft = release("v0.9.0", false, true);
        draft.draft = true;
        let releases = vec![
            release("v0.1.0", false, true),
            release("v0.10.0", false, true),
            release("v0.2.0", false, true),
            release("v0.3.0-rc.1", false, true),
            release("v0.4.0", true, true),
            release("nightly", false, true),
            draft,
        ];
        let tags = |cs: Vec<Candidate>| cs.into_iter().map(|c| c.tag).collect::<Vec<_>>();
        assert_eq!(tags(candidates(&releases, ASSET, false)), ["v0.10.0", "v0.2.0", "v0.1.0"]);
        assert_eq!(
            tags(candidates(&releases, ASSET, true)),
            ["v0.10.0", "v0.4.0", "v0.3.0-rc.1", "v0.2.0", "v0.1.0"],
            "a `-rc` tag counts as a prerelease even when GitHub does not flag it"
        );
    }

    #[test]
    fn latest_is_the_newest_release_built_for_this_platform() {
        let cs = candidates(&[release("v0.2.0", false, false), release("v0.1.1", false, true)], ASSET, false);
        assert_eq!(resolve(&cs, None).unwrap().tag, "v0.1.1");
        assert_eq!(cs[0].date, "2026-10-01");
    }

    #[test]
    fn a_named_version_matches_with_or_without_the_v() {
        let cs = candidates(&[release("v0.1.1", false, true), release("v0.1.0", false, true)], ASSET, false);
        assert_eq!(resolve(&cs, Some("0.1.0")).unwrap().tag, "v0.1.0");
        assert_eq!(resolve(&cs, Some("v0.1.0")).unwrap().tag, "v0.1.0");
        let missing = resolve(&cs, Some("0.0.9")).unwrap_err().to_string();
        assert!(missing.contains("v0.1.1, v0.1.0"), "{missing}");
        assert!(resolve(&cs, Some("latest-ish")).unwrap_err().to_string().contains("not a version"));
    }

    #[test]
    fn direction_compares_by_semver_not_text() {
        assert_eq!(direction(&v("0.1.1"), &v("0.1.1")), Direction::Same);
        assert_eq!(direction(&v("0.9.0"), &v("0.10.0")), Direction::Upgrade);
        assert_eq!(direction(&v("0.1.1"), &v("0.1.0")), Direction::Downgrade);
        assert_eq!(direction(&v("0.2.0"), &v("0.2.0-rc.1")), Direction::Downgrade);
    }

    #[test]
    fn a_select_answer_is_a_number_a_version_or_enter_for_the_first() {
        let cs = candidates(&[release("v0.1.1", false, true), release("v0.1.0", false, true)], ASSET, false);
        let listed: Vec<&Candidate> = cs.iter().collect();
        assert_eq!(pick(&listed, "\n").unwrap().tag, "v0.1.1");
        assert_eq!(pick(&listed, "2\n").unwrap().tag, "v0.1.0");
        assert_eq!(pick(&listed, " v0.1.0 ").unwrap().tag, "v0.1.0");
        assert!(pick(&listed, "3").is_none());
        assert!(pick(&listed, "0").is_none());
        assert!(pick(&listed, "0.0.1").is_none());
    }

    #[test]
    fn describe_marks_installed_latest_and_missing_builds() {
        let cs = candidates(&[release("v0.2.0", false, false), release("v0.1.1", false, true)], ASSET, false);
        let latest = latest_installable(&cs);
        assert!(describe(&cs[0], &v("0.1.1"), latest).contains("no build for this platform"));
        let line = describe(&cs[1], &v("0.1.1"), latest);
        assert!(line.contains("installed, latest"), "{line}");
    }

    #[test]
    fn a_daemon_inside_the_desktop_app_is_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("SenClaw Desktop.app");
        let mac = app.join("Contents").join("Resources").join("senclaw");
        assert_eq!(desktop_bundle_of(&mac), Some(app));

        let win = dir.path().join("SenClaw");
        std::fs::create_dir_all(&win).unwrap();
        std::fs::write(win.join("senclaw_desktop.exe"), b"").unwrap();
        assert_eq!(desktop_bundle_of(&win.join("senclaw.exe")), Some(win.clone()));

        let cli = dir.path().join(".senclaw").join("bin").join("senclaw");
        assert_eq!(desktop_bundle_of(&cli), None);
    }

    #[test]
    fn the_digest_prefix_is_stripped() {
        assert_eq!(sha256_hex(Some("sha256:abc")), Some("abc"));
        assert_eq!(sha256_hex(Some("md5:abc")), None);
        assert_eq!(sha256_hex(None), None);
    }
}

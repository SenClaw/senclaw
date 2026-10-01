//! `senclaw install desktop`, `senclaw uninstall desktop` — the native desktop
//! app, downloaded from the `desktop` repo's GitHub Releases.
//!
//! This is the first install only. Once the app is on disk it keeps itself
//! current with its own `update_desktop` helper, which replaces the bundle the
//! app is actually running from; nothing here is on that path.
//!
//! Asset names and the `latest.json` manifest must match the `desktop` repo's
//! release workflow (`.github/workflows/desktop.yml`).
//!
//! The platform is a plain `os` string (`std::env::consts::OS`) handed down
//! rather than `#[cfg]` items, so every platform's layout compiles and is
//! tested on whichever machine runs `cargo test`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde::Deserialize;

use super::distrib::{asset_url, binary_target, download, home, run_tool, tmp_dir};

/// The desktop bundle's own repo.
const DESKTOP_REPO: &str = "SenClaw/desktop";
/// Per-release manifest: which targets were built, and each bundle's sha256.
const MANIFEST_ASSET: &str = "latest.json";

const APP_BUNDLE_NAME: &str = "SenClaw Desktop.app";
const WINDOWS_SHORTCUT: &str = "SenClaw Desktop.lnk";
const LINUX_DESKTOP_ENTRY: &str = "senclaw-desktop.desktop";

/// Windows refuses to rename a folder while a process still runs from it, and
/// handles are released a moment after the process is killed.
const SWAP_ATTEMPTS: u32 = 5;
const SWAP_RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Subcommand, Debug)]
pub enum InstallCmd {
    /// Download the prebuilt SenClaw Desktop app for this platform and install it
    Desktop {
        /// Release tag to install (e.g. v0.3.0). Default: latest release.
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum UninstallCmd {
    /// Remove the SenClaw Desktop app installed by `senclaw install desktop`
    Desktop,
}

pub async fn run_install(cmd: InstallCmd) -> Result<()> {
    match cmd {
        InstallCmd::Desktop { version } => {
            let os = std::env::consts::OS;
            let target = default_install_target(os);
            install_desktop(version.as_deref(), &target, os).await?;
            println!("Installed {}", target.display());
            println!("{}", launch_hint(&target, os));
            Ok(())
        }
    }
}

pub async fn run_uninstall(cmd: UninstallCmd) -> Result<()> {
    match cmd {
        UninstallCmd::Desktop => uninstall_desktop(std::env::consts::OS),
    }
}

// ===== Release manifest =====

#[derive(Deserialize, Debug)]
struct Manifest {
    version: String,
    #[serde(default)]
    assets: HashMap<String, ManifestAsset>,
}

#[derive(Deserialize, Debug)]
struct ManifestAsset {
    sha256: String,
}

async fn fetch_manifest(version: Option<&str>) -> Result<Manifest> {
    let url = asset_url(DESKTOP_REPO, MANIFEST_ASSET, version);
    let client = reqwest::Client::builder().user_agent(format!("senclaw/{}", env!("CARGO_PKG_VERSION"))).build()?;
    let resp = client.get(&url).send().await.with_context(|| format!("cannot reach {url}"))?;
    if !resp.status().is_success() {
        bail!(
            "no desktop release found (HTTP {}) — {url}\ncheck https://github.com/{DESKTOP_REPO}/releases",
            resp.status()
        );
    }
    resp.json().await.with_context(|| format!("unreadable release manifest {url}"))
}

/// The sha256 of this platform's bundle, or an error naming what the release
/// does carry — a target the release never built (Intel macOS today) should
/// say so instead of surfacing as a bare 404 on the download.
fn bundle_checksum<'a>(manifest: &'a Manifest, triple: &str) -> Result<&'a str> {
    match manifest.assets.get(triple) {
        Some(asset) => Ok(&asset.sha256),
        None => {
            let mut built: Vec<&str> = manifest.assets.keys().map(String::as_str).collect();
            built.sort_unstable();
            bail!(
                "SenClaw Desktop {} has no build for {triple} (available: {}) — build it from source: \
                 https://github.com/{DESKTOP_REPO}",
                manifest.version,
                built.join(", ")
            )
        }
    }
}

/// Release asset holding the desktop bundle for `triple`.
fn bundle_asset_name(triple: &str, os: &str) -> String {
    match os {
        "macos" => format!("SenClaw-{triple}.app.zip"),
        "windows" => format!("SenClaw-{triple}.zip"),
        _ => format!("SenClaw-{triple}.tar.gz"),
    }
}

// ===== Install =====

async fn install_desktop(version: Option<&str>, target: &Path, os: &str) -> Result<()> {
    let triple = binary_target().context("SenClaw Desktop has no prebuilt bundle for this platform")?;
    let manifest = fetch_manifest(version).await?;
    let sha256 = bundle_checksum(&manifest, triple)?;
    println!("Installing SenClaw Desktop {}…", manifest.version);

    let asset = bundle_asset_name(triple, os);
    let staged = tmp_dir()?.join(&asset);
    // Pin the download to the manifest's own release: with no `--version`,
    // "latest" could move to a newer release between the two requests and the
    // checksum would then be for a different file.
    download(&asset_url(DESKTOP_REPO, &asset, Some(&manifest.version)), &staged).await?;
    let installed = verify_sha256(&staged, sha256).and_then(|()| install_bundle(&staged, target, os));
    let _ = std::fs::remove_file(&staged);
    installed
}

/// Put the downloaded archive `staged` in place at `target` and register it
/// with the desktop environment.
fn install_bundle(staged: &Path, target: &Path, os: &str) -> Result<()> {
    // Unix renames succeed with the files still open, so a first failure there
    // is real and retrying would only delay the error message.
    let attempts = if os == "windows" { SWAP_ATTEMPTS } else { 1 };
    if os == "windows" {
        // Reinstalling over a copy that is still running would fail the folder rename.
        stop_processes_in(target);
    }
    let mut attempt = 1;
    loop {
        match swap_bundle(staged, target, os) {
            Ok(()) => break,
            Err(e) if attempt < attempts => {
                println!(
                    "Install attempt {attempt}/{attempts} failed ({e:#}); retrying in {}s…",
                    SWAP_RETRY_DELAY.as_secs()
                );
                std::thread::sleep(SWAP_RETRY_DELAY);
                // Sweep again each time: a process can be spawned, or finish
                // dying, between attempts.
                stop_processes_in(target);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }

    match os {
        "windows" => create_windows_shortcuts(target),
        "linux" => write_linux_desktop_entry(target)?,
        _ => {}
    }
    Ok(())
}

/// Where `install desktop` puts the bundle.
fn default_install_target(os: &str) -> PathBuf {
    match os {
        "macos" => macos_app_dir().join(APP_BUNDLE_NAME),
        "windows" => {
            dirs::data_local_dir().unwrap_or_else(|| home().join("AppData").join("Local")).join("SenClaw").join("Desktop")
        }
        _ => home().join(".senclaw").join("desktop"),
    }
}

/// `/Applications`, or `~/Applications` when this user cannot write there.
fn macos_app_dir() -> PathBuf {
    let system = PathBuf::from("/Applications");
    let probe = system.join(".senclaw-write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            system
        }
        Err(_) => home().join("Applications"),
    }
}

/// The app's own executable inside an installed bundle.
fn app_executable(bundle: &Path, os: &str) -> PathBuf {
    match os {
        "macos" => bundle.join("Contents").join("MacOS").join("SenClaw Desktop"),
        "windows" => bundle.join("senclaw_desktop.exe"),
        _ => bundle.join("senclaw_desktop"),
    }
}

/// The daemon the app supervises, bundled next to it.
fn bundled_daemon(bundle: &Path, os: &str) -> PathBuf {
    match os {
        "macos" => bundle.join("Contents").join("Resources").join("senclaw"),
        "windows" => bundle.join("senclaw.exe"),
        _ => bundle.join("senclaw"),
    }
}

fn launch_hint(target: &Path, os: &str) -> String {
    match os {
        "macos" => format!("Launch it from Finder, Spotlight, or: open \"{}\"", target.display()),
        "windows" => format!("Launch it from the Start Menu, or: {}", app_executable(target, os).display()),
        _ => format!("Launch: {}", app_executable(target, os).display()),
    }
}

// ===== Bundle swap =====

/// Append `.<suffix>` to a path without touching its existing extension.
///
/// `Path::with_extension` is wrong here: on "SenClaw Desktop.app" it REPLACES
/// `.app`, yielding "SenClaw Desktop.new" — a sibling that macOS no longer
/// treats as a bundle.
fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

fn remove_path(p: &Path) -> std::io::Result<()> {
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else if p.exists() {
        std::fs::remove_file(p)
    } else {
        Ok(())
    }
}

/// Replace the desktop bundle at `target` with the contents of the downloaded
/// archive `staged`.
///
/// Every mutating step is a rename WITHIN `target`'s own directory, so none of
/// them can straddle a filesystem boundary or half-finish:
///
/// 1. extract → `<target>.new`   — `<target>` still untouched; a bad archive
///    or a full disk fails here and an installed app is pristine.
/// 2. `<target>` → `<target>.old`
/// 3. `<target>.new` → `<target>`
/// 4. remove `<target>.old`
///
/// A failure at (3) puts `.old` back rather than leaving the user with no app.
fn swap_bundle(staged: &Path, target: &Path, os: &str) -> Result<()> {
    let parent = target.parent().with_context(|| format!("{} has no parent directory", target.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;

    let new = with_suffix(target, "new");
    let old = with_suffix(target, "old");
    // Leftovers from a previous run that died mid-swap.
    let _ = remove_path(&new);
    let _ = remove_path(&old);

    if let Err(e) = extract_bundle(staged, &new, parent, os).and_then(|()| verify_bundle_payload(&new, os)) {
        // Nothing has moved yet — just don't leave a half-written `.new`.
        let _ = remove_path(&new);
        return Err(e);
    }

    let had_old = target.exists();
    if had_old {
        if let Err(e) = std::fs::rename(target, &old) {
            let _ = remove_path(&new);
            return Err(anyhow::Error::new(e).context(format!(
                "cannot replace {} — quit SenClaw Desktop if it is running, and check the folder is writable",
                target.display()
            )));
        }
    }

    if let Err(e) = std::fs::rename(&new, target) {
        if had_old {
            let _ = std::fs::rename(&old, target);
        }
        let _ = remove_path(&new);
        return Err(anyhow::Error::new(e)
            .context(format!("cannot move the new bundle into {} (rolled back)", target.display())));
    }

    if had_old {
        let _ = remove_path(&old);
    }
    Ok(())
}

/// Refuse to install a bundle missing the app or the daemon it supervises.
///
/// Runs on the freshly extracted `.new` copy, BEFORE an installed bundle is
/// moved aside — the one moment where rejecting costs nothing.
fn verify_bundle_payload(bundle: &Path, os: &str) -> Result<()> {
    let missing: Vec<String> = [app_executable(bundle, os), bundled_daemon(bundle, os)]
        .iter()
        .filter(|path| !path.is_file())
        .map(|path| path.strip_prefix(bundle).unwrap_or(path).display().to_string())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    bail!(
        "the downloaded bundle is incomplete — missing {}. Nothing was installed; this is a bad release build, \
         so report it rather than retrying (https://github.com/{DESKTOP_REPO}/releases)",
        missing.join(", ")
    )
}

fn extract_bundle(staged: &Path, new: &Path, parent: &Path, os: &str) -> Result<()> {
    match os {
        "macos" => {
            // Stage inside the TARGET's directory, not ~/.senclaw/tmp: the
            // final move must be a same-volume rename, and /Applications is
            // often on a different filesystem than $HOME.
            let stage = parent.join(".senclaw-install-stage");
            let _ = std::fs::remove_dir_all(&stage);
            std::fs::create_dir_all(&stage).with_context(|| {
                format!("cannot write to {} — the install location is not writable by this user", parent.display())
            })?;
            // `ditto` preserves symlinks, permissions, and code signatures — a
            // zip library does not, which breaks .app bundles.
            let result = run_tool("ditto", &["-xk", &staged.to_string_lossy(), &stage.to_string_lossy()]).and_then(
                |()| {
                    let app = stage.join(APP_BUNDLE_NAME);
                    if !app.exists() {
                        bail!("archive did not contain '{APP_BUNDLE_NAME}'");
                    }
                    std::fs::rename(&app, new).context("cannot stage the new bundle")
                },
            );
            let _ = std::fs::remove_dir_all(&stage);
            result
        }
        "windows" => {
            std::fs::create_dir_all(new).with_context(|| format!("cannot create {}", new.display()))?;
            crate::runtime::store::extract_zip(staged, new)
        }
        _ => {
            std::fs::create_dir_all(new).with_context(|| format!("cannot create {}", new.display()))?;
            crate::runtime::store::extract_tar_gz(staged, new)
        }
    }
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).context("read the downloaded bundle")?;
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("checksum mismatch for the downloaded bundle — refusing to install\n  expected {expected}\n  actual   {actual}");
    }
    Ok(())
}

// ===== Windows: release bundle locks + shortcuts =====

fn run_powershell(script: &str) -> bool {
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// PowerShell that force-stops everything holding the bundle at `dir` open.
///
/// Two sweeps:
///
/// 1. Any process whose IMAGE lives inside the bundle. Quitting the app kills
///    the daemon it spawned, but TerminateProcess does not cascade — the
///    daemon's MCP-server children (more `senclaw.exe` from the same file)
///    survive as orphans with the exe mapped, which blocks renaming the
///    folder forever. `Win32_Process.ExecutablePath` rather than
///    `Get-Process().Path`: the latter is null for any process PowerShell
///    cannot open, which is exactly the orphan being hunted.
/// 2. WebView2 helpers. They run from Program Files (so sweep 1 misses them)
///    but keep their user-data folder — inside the bundle by default —
///    locked; match those by command line.
///
/// The command-line sweep is deliberately limited to `msedgewebview2.exe`:
/// matching every process whose command line mentions the path would also
/// match any terminal the user happened to type the path into. `$PID` (the
/// PowerShell running this) and `self_pid` (this `senclaw`) are spared — a
/// `senclaw.exe` run from inside the bundle must survive its own sweep.
fn locker_kill_script(dir: &str, self_pid: u32) -> String {
    let dir = dir.replace('\'', "''");
    format!(
        "$t='{dir}'; $self={self_pid}; \
         Get-CimInstance Win32_Process | Where-Object {{ \
           ($_.ExecutablePath -and $_.ExecutablePath.StartsWith($t,'OrdinalIgnoreCase')) \
           -or ($_.Name -eq 'msedgewebview2.exe' -and $_.CommandLine \
                -and $_.CommandLine.ToLower().Contains($t.ToLower())) }} | ForEach-Object {{ \
           if ($_.ProcessId -ne $self -and $_.ProcessId -ne $PID) {{ \
             taskkill /PID $_.ProcessId /T /F 2>&1 | Out-Null \
           }} }}"
    )
}

/// PowerShell that (re)creates the shortcut on the Desktop and in the Start
/// Menu, pointing at `exe`. Overwrites in place, so a reinstall refreshes it.
fn shortcut_script(exe: &str, dir: &str) -> String {
    let exe = exe.replace('\'', "''");
    let dir = dir.replace('\'', "''");
    format!(
        "$ws = New-Object -ComObject WScript.Shell; \
         foreach ($p in @([Environment]::GetFolderPath('Desktop'), \
                          (Join-Path ([Environment]::GetFolderPath('StartMenu')) 'Programs'))) {{ \
           $s = $ws.CreateShortcut((Join-Path $p '{WINDOWS_SHORTCUT}')); \
           $s.TargetPath = '{exe}'; $s.WorkingDirectory = '{dir}'; \
           $s.IconLocation = '{exe},0'; $s.Save() \
         }}"
    )
}

/// Best-effort: stop every process still running from the bundle at `target`.
fn stop_processes_in(target: &Path) {
    if !target.exists() {
        return;
    }
    println!("Closing processes still running from {}…", target.display());
    let _ = run_powershell(&locker_kill_script(&target.to_string_lossy(), std::process::id()));
    // Handles release asynchronously after TerminateProcess.
    std::thread::sleep(Duration::from_millis(500));
}

/// Best-effort Desktop + Start Menu shortcuts.
fn create_windows_shortcuts(target: &Path) {
    let exe = app_executable(target, "windows");
    if run_powershell(&shortcut_script(&exe.to_string_lossy(), &target.to_string_lossy())) {
        println!("Shortcuts created: Desktop + Start Menu → SenClaw Desktop");
    } else {
        println!("Note: could not create the Desktop/Start Menu shortcuts — launch {} directly.", exe.display());
    }
}

/// The two places [`shortcut_script`] writes to.
fn windows_shortcut_paths() -> Vec<PathBuf> {
    let start_menu =
        dirs::data_dir().map(|d| d.join("Microsoft").join("Windows").join("Start Menu").join("Programs"));
    [dirs::desktop_dir(), start_menu].into_iter().flatten().map(|dir| dir.join(WINDOWS_SHORTCUT)).collect()
}

// ===== Linux: launcher entry =====

fn linux_desktop_entry_path() -> PathBuf {
    home().join(".local").join("share").join("applications").join(LINUX_DESKTOP_ENTRY)
}

fn linux_desktop_entry(bundle_dir: &Path) -> String {
    // Quoted: the Desktop Entry spec splits an unquoted Exec on spaces.
    format!(
        "[Desktop Entry]\nType=Application\nName=SenClaw Desktop\nExec=\"{}\"\nTerminal=false\nCategories=Utility;\n",
        app_executable(bundle_dir, "linux").display()
    )
}

fn write_linux_desktop_entry(bundle_dir: &Path) -> Result<()> {
    let entry = linux_desktop_entry_path();
    if let Some(dir) = entry.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(&entry, linux_desktop_entry(bundle_dir)).with_context(|| format!("write {}", entry.display()))?;
    println!("Desktop entry: {}", entry.display());
    Ok(())
}

// ===== Uninstall =====

/// Everything `install desktop` may have put on this machine. Only the app
/// itself — `~/.senclaw` (chats, settings, models) is the daemon's and stays.
fn installed_paths(os: &str) -> Vec<PathBuf> {
    match os {
        // Both: `macos_app_dir` picks whichever was writable at install time.
        "macos" => vec![
            PathBuf::from("/Applications").join(APP_BUNDLE_NAME),
            home().join("Applications").join(APP_BUNDLE_NAME),
        ],
        "windows" => {
            let mut paths = vec![default_install_target(os)];
            paths.extend(windows_shortcut_paths());
            paths
        }
        _ => vec![default_install_target(os), linux_desktop_entry_path()],
    }
}

fn uninstall_desktop(os: &str) -> Result<()> {
    if os == "windows" {
        // A folder holding a running exe cannot be removed.
        stop_processes_in(&default_install_target(os));
    }
    let mut removed = false;
    for path in installed_paths(os) {
        if path.exists() {
            remove_path(&path).with_context(|| format!("cannot remove {}", path.display()))?;
            println!("Removed {}", path.display());
            removed = true;
        }
    }
    if !removed {
        println!("SenClaw Desktop is not installed (nothing to remove).");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A `.tar.gz` shaped like the Linux release bundle (`tar -C <dir> -czf … .`).
    fn linux_bundle(dir: &Path, files: &[&str]) -> PathBuf {
        let archive = dir.join("SenClaw-x86_64-unknown-linux-gnu.tar.gz");
        let gz = flate2::write::GzEncoder::new(std::fs::File::create(&archive).unwrap(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for name in files {
            let body = format!("contents of {name}");
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, format!("./{name}"), body.as_bytes()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        archive
    }

    /// A `.zip` shaped like the Windows release bundle (files at the root).
    fn windows_bundle(dir: &Path, files: &[&str]) -> PathBuf {
        let archive = dir.join("SenClaw-x86_64-pc-windows-msvc.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        for name in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(format!("contents of {name}").as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        archive
    }

    fn manifest(json: &str) -> Manifest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn asset_names_match_the_desktop_release_workflow() {
        assert_eq!(bundle_asset_name("aarch64-apple-darwin", "macos"), "SenClaw-aarch64-apple-darwin.app.zip");
        assert_eq!(bundle_asset_name("x86_64-pc-windows-msvc", "windows"), "SenClaw-x86_64-pc-windows-msvc.zip");
        assert_eq!(bundle_asset_name("x86_64-unknown-linux-gnu", "linux"), "SenClaw-x86_64-unknown-linux-gnu.tar.gz");
    }

    #[test]
    fn the_checksum_comes_from_the_release_manifest_as_published() {
        // The shape desktop.yml writes, extra fields and all.
        let m = manifest(
            r#"{"version":"0.1.1","publishedAt":"2026-09-27T12:49:19Z","notes":"- x","minVersion":"0.0.0",
                "assets":{"x86_64-pc-windows-msvc":{"name":"SenClaw-x86_64-pc-windows-msvc.zip","size":5,"sha256":"abc"}}}"#,
        );
        assert_eq!(bundle_checksum(&m, "x86_64-pc-windows-msvc").unwrap(), "abc");
    }

    #[test]
    fn a_target_the_release_never_built_is_named_with_what_was() {
        let m = manifest(
            r#"{"version":"0.1.1","assets":{"x86_64-pc-windows-msvc":{"sha256":"a"},"aarch64-apple-darwin":{"sha256":"b"}}}"#,
        );
        let err = bundle_checksum(&m, "x86_64-apple-darwin").unwrap_err().to_string();
        assert!(err.contains("no build for x86_64-apple-darwin"), "{err}");
        assert!(err.contains("aarch64-apple-darwin, x86_64-pc-windows-msvc"), "{err}");
    }

    #[test]
    fn a_wrong_checksum_refuses_the_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("bundle");
        std::fs::write(&file, b"hello").unwrap();
        // sha256("hello")
        let good = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        verify_sha256(&file, good).unwrap();
        verify_sha256(&file, &good.to_uppercase()).unwrap();
        let err = verify_sha256(&file, &good.replace('2', "3")).unwrap_err().to_string();
        assert!(err.contains("refusing to install"), "{err}");
    }

    #[test]
    fn a_fresh_install_lands_in_the_target_and_leaves_nothing_beside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = linux_bundle(tmp.path(), &["senclaw_desktop", "senclaw", "lib/libapp.so"]);
        let target = tmp.path().join("apps").join("desktop");

        swap_bundle(&staged, &target, "linux").unwrap();

        assert!(target.join("senclaw_desktop").is_file());
        assert!(target.join("lib").join("libapp.so").is_file());
        let siblings: Vec<_> =
            std::fs::read_dir(target.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(siblings, ["desktop"], "no .new/.old left behind");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(target.join("senclaw_desktop")).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "the app must stay executable");
        }
    }

    #[test]
    fn reinstalling_replaces_the_old_bundle_wholesale() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("Desktop");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("stale.dll"), b"from the previous version").unwrap();
        let staged = windows_bundle(tmp.path(), &["senclaw_desktop.exe", "senclaw.exe", "data/app.so"]);

        swap_bundle(&staged, &target, "windows").unwrap();

        assert!(target.join("senclaw_desktop.exe").is_file());
        assert!(target.join("data").join("app.so").is_file());
        assert!(!target.join("stale.dll").exists(), "files the new version dropped must not linger");
        assert!(!with_suffix(&target, "old").exists());
    }

    #[test]
    fn an_incomplete_bundle_leaves_the_installed_app_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("desktop");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("senclaw_desktop"), b"working install").unwrap();
        // No daemon: a desktop app that would never start.
        let staged = linux_bundle(tmp.path(), &["senclaw_desktop"]);

        let err = swap_bundle(&staged, &target, "linux").unwrap_err().to_string();

        assert!(err.contains("missing senclaw"), "{err}");
        assert_eq!(std::fs::read(target.join("senclaw_desktop")).unwrap(), b"working install");
        assert!(!with_suffix(&target, "new").exists());
    }

    #[test]
    fn the_app_bundle_suffix_is_appended_not_swapped_for_the_extension() {
        assert_eq!(
            with_suffix(Path::new("/Applications/SenClaw Desktop.app"), "new"),
            Path::new("/Applications/SenClaw Desktop.app.new")
        );
    }

    #[test]
    fn the_locker_sweep_spares_this_process_and_escapes_the_path() {
        let script = locker_kill_script(r"C:\Users\O'Brien\AppData\Local\SenClaw\Desktop", 4242);
        assert!(script.contains(r"$t='C:\Users\O''Brien\AppData\Local\SenClaw\Desktop'"), "{script}");
        assert!(script.contains("$self=4242"));
        assert!(script.contains("$_.ProcessId -ne $self -and $_.ProcessId -ne $PID"));
    }

    #[test]
    fn the_shortcut_points_at_the_app_and_uninstall_knows_its_name() {
        let script = shortcut_script(r"C:\SenClaw\Desktop\senclaw_desktop.exe", r"C:\SenClaw\Desktop");
        assert!(script.contains(r"$s.TargetPath = 'C:\SenClaw\Desktop\senclaw_desktop.exe'"), "{script}");
        assert!(script.contains("'SenClaw Desktop.lnk'"), "{script}");
        assert!(windows_shortcut_paths().iter().all(|p| p.ends_with(WINDOWS_SHORTCUT)));
    }

    #[test]
    fn the_linux_launcher_quotes_a_path_with_spaces() {
        let entry = linux_desktop_entry(Path::new("/home/a b/.senclaw/desktop"));
        assert!(entry.contains("Exec=\"/home/a b/.senclaw/desktop/senclaw_desktop\"\n"), "{entry}");
    }

    /// The one test that reaches the network: the real latest release, through
    /// the real code path, into a scratch directory.
    #[tokio::test]
    #[ignore = "downloads the latest SenClaw Desktop release (~60 MB)"]
    async fn the_published_release_installs_on_this_platform() {
        let os = std::env::consts::OS;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(if os == "macos" { APP_BUNDLE_NAME } else { "desktop" });

        // `swap_bundle`, not `install_bundle`: no shortcuts or launcher entries
        // written into the real profile of whoever runs this.
        let triple = binary_target().unwrap();
        let manifest = fetch_manifest(None).await.unwrap();
        let asset = bundle_asset_name(triple, os);
        let staged = tmp.path().join(&asset);
        download(&asset_url(DESKTOP_REPO, &asset, Some(&manifest.version)), &staged).await.unwrap();
        verify_sha256(&staged, bundle_checksum(&manifest, triple).unwrap()).unwrap();
        swap_bundle(&staged, &target, os).unwrap();

        let out = std::process::Command::new(bundled_daemon(&target, os)).arg("--version").output().unwrap();
        assert!(out.status.success(), "the bundled daemon must run: {out:?}");
    }
}

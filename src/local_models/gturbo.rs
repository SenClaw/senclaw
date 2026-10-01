//! The TurboFieldfare checkpoint is not an MLX snapshot.
//!
//! `TurboFieldfareRepack` always fetches one pinned Hugging Face revision
//! (`SupportedModelSource` in the engine) and writes a `.gturbo` directory.
//! A raw snapshot of the same repo will not load. Download therefore runs
//! the `TurboFieldfareRepack` binary shipped in the installed
//! `sen-turbo-fieldfare` package — never the generic Hugging Face copier.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::runtime::store::InstalledPackage;
#[cfg(test)]
use crate::runtime::store::PackageSource;
use crate::runtime::version::cmp_versions;

use super::download::{DownloadState, DownloadStatus};

/// Keep in lockstep with TurboFieldfareRepack `SupportedModelSource`.
pub const REPO_ID: &str = "mlx-community/gemma-4-26b-a4b-it-4bit";
pub const REVISION: &str = "0d77464eeb233a2da68ebf9d7dc4edaac7db956d";
pub const SOURCE_INDEX_SHA256: &str =
    "bf198c9f5ea6462addca1966e5dd669c407537a876e82cf06db9084c5c850b13";
/// Bytes the installer expects to transfer. Progress percents scale against this.
pub const APPROXIMATE_DOWNLOAD_BYTES: u64 = 14_620_479_420;
pub const TEXT_DIR_NAME: &str = "gemma4.gturbo";

const RUNTIME_ID: &str = "sen-turbo-fieldfare";
const REPACK_NAME: &str = "TurboFieldfareRepack";
const MANIFEST_CAP: u64 = 4 * 1024 * 1024;

#[derive(Debug)]
pub enum StartError {
    AlreadyInstalled,
    Foreign(PathBuf),
    NeedsTextModel,
    CorruptPartial(PathBuf),
    Message(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyInstalled => write!(f, "Gemma 4 26B-A4B is already installed"),
            Self::Foreign(path) => write!(
                f,
                "{} is not the pinned Gemma 4 TurboFieldfare install. Remove that directory before downloading.",
                path.display()
            ),
            Self::NeedsTextModel => {
                write!(f, "install the Gemma 4 text model before the image pack")
            }
            Self::CorruptPartial(path) => write!(
                f,
                "the saved download beside {} is incomplete. Remove {}.partial and {}.resume.json, then try again.",
                path.display(),
                path.display(),
                path.display()
            ),
            Self::Message(msg) => write!(f, "{msg}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RepackPlan {
    pub output: PathBuf,
    pub text_model: Option<PathBuf>,
    pub resume: bool,
    pub total_bytes: u64,
    pub file_label: String,
}

pub enum RepackOutcome {
    Finished,
    Cancelled,
}

/// Reject a download that is not the pinned checkpoint. `repo` and
/// `revision` may be omitted; a present value must be the pin. There is no
/// file picker — the installer chooses the bytes.
pub fn check_request(repo: Option<&str>, revision: Option<&str>, file: Option<&str>) -> Result<(), String> {
    if file.is_some() {
        return Err(
            "a TurboFieldfare download does not take a file name; it always fetches the pinned Gemma 4 checkpoint"
                .into(),
        );
    }
    if let Some(repo) = repo.map(str::trim).filter(|s| !s.is_empty()) {
        let repo = repo.trim_matches('/');
        if !repo.eq_ignore_ascii_case(REPO_ID) {
            return Err(format!("TurboFieldfare only installs {REPO_ID}"));
        }
    }
    if let Some(revision) = revision.map(str::trim).filter(|s| !s.is_empty()) {
        if revision != REVISION {
            return Err(format!("TurboFieldfare only installs revision {REVISION}"));
        }
    }
    Ok(())
}

pub fn text_dir_matches_pin(dir: &Path) -> bool {
    identity_error(dir).is_none()
}

/// `None` when `dir` is the pinned text install. `Some` is why it is not.
pub fn identity_error(dir: &Path) -> Option<String> {
    let Some(manifest) = read_manifest(dir) else {
        return Some(format!("no TurboFieldfare manifest in {}", dir.display()));
    };
    if manifest.get("magic").and_then(|v| v.as_str()) != Some("GTURBO") {
        return Some(format!("{} is not a TurboFieldfare text model", dir.display()));
    }
    let model_id = manifest.get("modelID").and_then(|v| v.as_str()).unwrap_or("");
    if model_id != REPO_ID {
        return Some(format!("{dir} is {model_id}, not {REPO_ID}", dir = dir.display()));
    }
    let hash = manifest.get("sourceSnapshotHash").and_then(|v| v.as_str()).unwrap_or("");
    if !snapshot_matches(hash) {
        return Some(format!(
            "{dir} snapshot {hash} is not {sha}",
            dir = dir.display(),
            sha = SOURCE_INDEX_SHA256
        ));
    }
    if !dir.join("model_weights.bin").is_file() {
        return Some(format!("{} is missing model_weights.bin", dir.display()));
    }
    None
}

/// Newest installed `sen-turbo-fieldfare` package that actually contains
/// `bin/TurboFieldfareRepack`.
pub fn repack_binary(packages: &[InstalledPackage]) -> Result<PathBuf, String> {
    let mut best: Option<&InstalledPackage> = None;
    for package in packages.iter().filter(|p| p.manifest.id == RUNTIME_ID) {
        best = Some(match best {
            None => package,
            Some(current) if cmp_versions(&package.manifest.version, &current.manifest.version) == Ordering::Greater => {
                package
            }
            Some(current) => current,
        });
    }
    let Some(package) = best else {
        return Err(
            "install the sen-turbo-fieldfare runtime before downloading Gemma 4. The download runs TurboFieldfareRepack from that package."
                .into(),
        );
    };
    let bin = package.dir.join("bin").join(REPACK_NAME);
    if !bin.is_file() {
        return Err(format!(
            "TurboFieldfareRepack is missing from {}. Reinstall sen-turbo-fieldfare.",
            package.dir.display()
        ));
    }
    Ok(bin)
}

pub fn plan(root: &Path, vision: bool) -> Result<RepackPlan, StartError> {
    if vision {
        let text = find_text_install(root).ok_or(StartError::NeedsTextModel)?;
        let stem = text
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".gturbo"))
            .ok_or_else(|| StartError::Message(format!("{} is not a .gturbo directory", text.display())))?;
        let output = text.with_file_name(format!("{stem}.vision.gturbo"));
        let resume = resume_flag(&output)?;
        return Ok(RepackPlan {
            file_label: output.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            output,
            text_model: Some(text),
            resume,
            total_bytes: 0,
        });
    }
    if find_text_install(root).is_some() {
        return Err(StartError::AlreadyInstalled);
    }
    let output = root.join(TEXT_DIR_NAME);
    if output.exists() {
        return Err(StartError::Foreign(output));
    }
    let resume = resume_flag(&output)?;
    Ok(RepackPlan {
        file_label: TEXT_DIR_NAME.into(),
        output,
        text_model: None,
        resume,
        total_bytes: APPROXIMATE_DOWNLOAD_BYTES,
    })
}

/// A finished text install anywhere directly under `root`. The canonical
/// name `gemma4.gturbo` wins when more than one directory matches the pin.
pub fn find_text_install(root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut found = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() && is_text_dir_name(&name) && text_dir_matches_pin(&path) {
            found.push(path);
        }
    }
    found.sort_by(|a, b| {
        let a_canonical = a.file_name().and_then(|n| n.to_str()) == Some(TEXT_DIR_NAME);
        let b_canonical = b.file_name().and_then(|n| n.to_str()) == Some(TEXT_DIR_NAME);
        b_canonical.cmp(&a_canonical).then_with(|| a.cmp(b))
    });
    found.into_iter().next()
}

pub fn vision_manifest_present(dir: &Path) -> bool {
    read_manifest(dir).and_then(|v| v.get("magic").and_then(|m| m.as_str()).map(|s| s == "GTURBO-VISION")).unwrap_or(false)
}

/// Run the repack binary. `state` receives percent lines the installer
/// prints on stderr (`[install] … (N%)`). Cancel sends SIGTERM; a later
/// download passes `--resume` when both the partial directory and the
/// checkpoint file are still on disk.
pub async fn run_repack(
    bin: &Path,
    plan: &RepackPlan,
    state: &Arc<Mutex<DownloadState>>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<RepackOutcome> {
    let mut cmd = Command::new(bin);
    if let Some(text) = &plan.text_model {
        cmd.arg("--vision-output").arg(&plan.output).arg("--text-model").arg(text);
    } else {
        cmd.arg("--output").arg(&plan.output);
    }
    if plan.resume {
        cmd.arg("--resume");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = bin.parent() {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().with_context(|| format!("spawn {}", bin.display()))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let log: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let total = plan.total_bytes;
    let stdout_task = tokio::spawn(pump(stdout, Arc::clone(state), Arc::clone(&log), total));
    let stderr_task = tokio::spawn(pump(stderr, Arc::clone(state), Arc::clone(&log), total));

    let status = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Ok(RepackOutcome::Cancelled);
        }
        status = child.wait() => status,
    };
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    let status = status.context("wait for TurboFieldfareRepack")?;
    if !status.success() {
        let tail = log.lock().unwrap_or_else(|e| e.into_inner()).clone();
        bail!("TurboFieldfareRepack exited {}.\n{tail}", status.code().unwrap_or(-1));
    }
    if plan.text_model.is_some() {
        if !vision_manifest_present(&plan.output) {
            bail!("image pack install finished but {} has no GTURBO-VISION manifest", plan.output.display());
        }
    } else if let Some(why) = identity_error(&plan.output) {
        bail!("install finished but the directory is not the pinned checkpoint: {why}");
    }
    Ok(RepackOutcome::Finished)
}

pub fn parse_install_percent(line: &str) -> Option<u8> {
    let open = line.rfind('(')?;
    let rest = &line[open + 1..];
    let close = rest.find("%)")?;
    let pct: u8 = rest[..close].parse().ok()?;
    (pct <= 100).then_some(pct)
}

fn resume_flag(output: &Path) -> Result<bool, StartError> {
    if is_complete_output(output) {
        return Err(StartError::AlreadyInstalled);
    }
    if output.exists() {
        return Err(StartError::Foreign(output.to_path_buf()));
    }
    let partial = partial_dir(output);
    let checkpoint = checkpoint_file(output);
    match (partial.is_dir(), checkpoint.is_file()) {
        (true, true) => Ok(true),
        (false, false) => Ok(false),
        _ => Err(StartError::CorruptPartial(output.to_path_buf())),
    }
}

fn is_complete_output(output: &Path) -> bool {
    if vision_dir_name(output) {
        vision_manifest_present(output)
    } else {
        text_dir_matches_pin(output)
    }
}

fn vision_dir_name(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".vision.gturbo"))
}

fn partial_dir(output: &Path) -> PathBuf {
    PathBuf::from(format!("{}.partial", output.display()))
}

fn checkpoint_file(output: &Path) -> PathBuf {
    PathBuf::from(format!("{}.resume.json", output.display()))
}

pub fn is_text_dir_name(name: &str) -> bool {
    name.ends_with(".gturbo") && !name.ends_with(".vision.gturbo") && name.len() > ".gturbo".len()
}

fn snapshot_matches(value: &str) -> bool {
    let hex = value.strip_prefix("sha256:").unwrap_or(value);
    hex.eq_ignore_ascii_case(SOURCE_INDEX_SHA256)
}

fn read_manifest(dir: &Path) -> Option<serde_json::Value> {
    let path = dir.join("manifest.json");
    let len = std::fs::metadata(&path).ok()?.len();
    if len > MANIFEST_CAP {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

async fn pump(
    reader: Option<impl tokio::io::AsyncRead + Unpin>,
    state: Arc<Mutex<DownloadState>>,
    log: Arc<Mutex<String>>,
    total: u64,
) {
    let Some(reader) = reader else { return };
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(pct) = parse_install_percent(&line) {
            let mut current = state.lock().unwrap_or_else(|e| e.into_inner());
            if !current.is_finished() {
                current.state = DownloadStatus::Downloading;
                current.percent = Some(f64::from(pct));
                if total > 0 {
                    current.total_bytes = total;
                    current.received_bytes = total.saturating_mul(u64::from(pct)) / 100;
                }
            }
        }
        let mut buf = log.lock().unwrap_or_else(|e| e.into_inner());
        if buf.len() > 6000 {
            let drain = buf.len() - 4000;
            *buf = buf[drain..].to_string();
        }
        buf.push_str(&line);
        buf.push('\n');
    }
}

/// Test helper so a unit test can build an [`InstalledPackage`] without the
/// installer. Not used at runtime.
#[cfg(test)]
pub fn package_for_test(dir: PathBuf, version: &str) -> InstalledPackage {
    let manifest: sen_runtime_sdk::manifest::RuntimeManifest = serde_json::from_str(&format!(
        r#"{{
            "schemaVersion": 1,
            "id": "sen-turbo-fieldfare",
            "name": "SenClaw TurboFieldfare",
            "version": "{version}",
            "type": "llm-engine",
            "slots": ["gturbo"],
            "platforms": ["darwin-arm64"],
            "mode": "model",
            "entry": {{ "command": "bin/sen-turbo-fieldfare", "args": ["serve"] }}
        }}"#
    ))
    .expect("manifest");
    InstalledPackage { manifest, warnings: Vec::new(), dir, source: PackageSource::Local }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin_manifest() -> String {
        format!(
            r#"{{"magic":"GTURBO","modelID":"{REPO_ID}","sourceSnapshotHash":"sha256:{SOURCE_INDEX_SHA256}"}}"#
        )
    }

    #[test]
    fn request_must_be_the_pinned_checkpoint() {
        assert!(check_request(None, None, None).is_ok());
        assert!(check_request(Some(REPO_ID), Some(REVISION), None).is_ok());
        assert!(check_request(Some("org/other"), None, None).is_err());
        assert!(check_request(Some(REPO_ID), Some("main"), None).is_err());
        assert!(check_request(Some(REPO_ID), Some(REVISION), Some("model.safetensors")).is_err());
    }

    #[test]
    fn percent_lines_from_the_installer_parse() {
        assert_eq!(parse_install_percent("[install] 7.3 GiB of 13.6 GiB (50%)"), Some(50));
        assert_eq!(parse_install_percent("[install] reading source metadata"), None);
        assert_eq!(parse_install_percent("[install] 1.0 GiB of 13.6 GiB (100%), 1.0 GiB already on disk"), Some(100));
    }

    #[test]
    fn a_foreign_directory_is_not_resumed_over() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(TEXT_DIR_NAME);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), r#"{"magic":"GTURBO","modelID":"other/model"}"#).unwrap();
        std::fs::write(dir.join("model_weights.bin"), b"w").unwrap();
        assert!(matches!(plan(tmp.path(), false), Err(StartError::Foreign(_))));
    }

    #[test]
    fn a_matching_install_is_not_downloaded_again() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(TEXT_DIR_NAME);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), pin_manifest()).unwrap();
        std::fs::write(dir.join("model_weights.bin"), b"w").unwrap();
        assert!(matches!(plan(tmp.path(), false), Err(StartError::AlreadyInstalled)));
    }

    #[test]
    fn a_saved_partial_uses_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join(TEXT_DIR_NAME);
        std::fs::create_dir_all(format!("{}.partial", output.display())).unwrap();
        std::fs::write(format!("{}.resume.json", output.display()), b"{}").unwrap();
        let plan = plan(tmp.path(), false).unwrap();
        assert!(plan.resume);
        assert_eq!(plan.output, output);
        assert!(plan.text_model.is_none());
    }

    #[test]
    fn repack_binary_comes_from_the_newest_installed_package() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("0.1.0");
        let new = tmp.path().join("0.2.0");
        std::fs::create_dir_all(old.join("bin")).unwrap();
        std::fs::create_dir_all(new.join("bin")).unwrap();
        std::fs::write(old.join("bin").join(REPACK_NAME), b"old").unwrap();
        std::fs::write(new.join("bin").join(REPACK_NAME), b"new").unwrap();
        let packages = vec![package_for_test(old, "0.1.0"), package_for_test(new.clone(), "0.2.0")];
        let bin = repack_binary(&packages).unwrap();
        assert_eq!(bin, new.join("bin").join(REPACK_NAME));
        assert!(repack_binary(&[]).is_err());
    }

    #[test]
    fn the_on_disk_gemma4_install_matches_the_pin_when_present() {
        let Some(home) = std::env::var_os("HOME") else { return };
        let dir = PathBuf::from(home).join("Library/Application Support/TurboFieldfare/gemma4.gturbo");
        if !dir.join("manifest.json").is_file() {
            return;
        }
        assert!(text_dir_matches_pin(&dir), "{:?}", identity_error(&dir));
    }
}

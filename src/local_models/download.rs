//! Pulling a checkpoint from HuggingFace, with progress and resume
//! (`docs/runtime-protocol.md` §5.3). Ported from the old `local-model-core`
//! app's downloader: what matters is not the HTTP, it is that a multi-GB
//! download survives being interrupted, reports enough for a progress bar to
//! be honest, and can be cancelled without leaving a half-written shard that
//! later looks like a complete model.
//!
//! MLX downloads the whole repo snapshot (minus formats no local engine reads);
//! GGUF downloads exactly the requested file(s) — a repo commonly holds many
//! quantizations, and only one is wanted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

const HF_BASE: &str = "https://huggingface.co";

/// `docs/runtime-protocol.md` §5.3: `queued | listing | downloading | done |
/// failed | cancelled` — pinned to match the install-job vocabulary
/// (`JobState`); an earlier `Error` here diverged for no reason and broke
/// both clients' failure-state matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadStatus {
    Queued,
    Listing,
    Downloading,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadState {
    pub download_id: String,
    pub repo: String,
    pub files: Vec<String>,
    pub format: String,
    pub state: DownloadStatus,
    pub received_bytes: u64,
    pub total_bytes: u64,
    pub percent: Option<f64>,
    pub error: Option<String>,
    pub started_at: u64,
    pub finished_at: Option<u64>,
}

impl DownloadState {
    fn new(download_id: &str, repo: &str, format: &str) -> Self {
        Self {
            download_id: download_id.to_string(),
            repo: repo.to_string(),
            files: Vec::new(),
            format: format.to_string(),
            state: DownloadStatus::Queued,
            received_bytes: 0,
            total_bytes: 0,
            percent: None,
            error: None,
            started_at: now_millis(),
            finished_at: None,
        }
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, DownloadStatus::Done | DownloadStatus::Failed | DownloadStatus::Cancelled)
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Clone)]
struct Handle {
    state: Arc<Mutex<DownloadState>>,
    cancel: CancellationToken,
}

fn registry() -> &'static Mutex<HashMap<String, Handle>> {
    static R: OnceLock<Mutex<HashMap<String, Handle>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Files that are not weights and not needed to run the model — noise
/// (`.gitattributes`) or an expensive duplicate format neither engine here
/// reads. GGUF downloads never reach this filter (they name their file
/// explicitly); it applies to the MLX whole-snapshot download only.
fn should_skip_for_mlx(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    if base.starts_with('.') {
        return true;
    }
    if lower.starts_with("onnx/") || lower.starts_with("coreml/") || lower.starts_with("openvino/") || lower.starts_with("gguf/") {
        return true;
    }
    matches!(base, "pytorch_model.bin" | "tf_model.h5" | "model.ckpt.index" | "flax_model.msgpack" | "readme.md" | "license" | "license.txt")
        || base.ends_with(".onnx")
        || base.ends_with(".pth")
        || base.ends_with(".gguf")
        || base.ends_with(".png")
        || base.ends_with(".jpg")
        || base.ends_with(".gif")
}

#[derive(Deserialize)]
struct TreeEntry {
    #[serde(rename = "type")]
    entry_type: String,
    path: String,
    #[serde(default)]
    size: u64,
}

/// Where a download's files land: MLX keeps the whole-repo snapshot layout;
/// GGUF is flat under `gguf/<org>__<repo>/`.
fn dest_dir(local_models_dir: &Path, format: &str, repo_dirname: &str) -> PathBuf {
    if format == "gguf" {
        local_models_dir.join("gguf").join(repo_dirname)
    } else {
        local_models_dir.join(repo_dirname)
    }
}

/// Where one wanted file lands under `dest_root`. GGUF takes only the
/// basename — a GGUF entry never needs subdirectories, so there is nothing to
/// validate. MLX keeps the repo's own tree shape, but only after checking it
/// through the same guard the archive extractor uses
/// (`runtime::store::safe_relative_path`): HuggingFace's tree API is an
/// external input, and a git tree entry name cannot actually contain `..`
/// today, but the join should not trust that assumption forever.
fn resolve_download_dest(dest_root: &Path, format: &str, entry_path: &str) -> Result<PathBuf> {
    if format == "gguf" {
        Ok(dest_root.join(entry_path.rsplit('/').next().unwrap_or(entry_path)))
    } else {
        let safe_rel = crate::runtime::store::safe_relative_path(Path::new(entry_path))
            .with_context(|| format!("`{entry_path}` is not a safe path to extract to"))?;
        Ok(dest_root.join(safe_rel))
    }
}

/// Start a download, or return the state of one already running for this
/// exact target. Idempotent on purpose: the UI polls, and a double-click on
/// Download must not start a second writer against the same files.
pub fn start(
    local_models_dir: &Path,
    repo: &str,
    format: &str,
    revision: &str,
    gguf_file: Option<String>,
    mmproj_file: Option<String>,
) -> DownloadState {
    let target_key = format!("{format}:{repo}:{}", gguf_file.as_deref().unwrap_or(""));
    {
        let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = reg.values().find(|h| {
            let s = h.state.lock().unwrap_or_else(|e| e.into_inner());
            !s.is_finished() && format!("{}:{}:{}", s.format, s.repo, s.files.first().cloned().unwrap_or_default()) == target_key
        }) {
            return h.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        }
    }

    let download_id = uuid::Uuid::new_v4().to_string();
    let state = Arc::new(Mutex::new(DownloadState::new(&download_id, repo, format)));
    let cancel = CancellationToken::new();
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(download_id.clone(), Handle { state: Arc::clone(&state), cancel: cancel.clone() });

    let (repo, revision, format, dir) = (repo.to_string(), revision.to_string(), format.to_string(), local_models_dir.to_path_buf());
    let st = Arc::clone(&state);
    tokio::spawn(async move {
        let result = run(&repo, &revision, &format, gguf_file, mmproj_file, &dir, Arc::clone(&st), cancel).await;
        let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
        if s.state != DownloadStatus::Cancelled {
            match result {
                Ok(()) => s.state = DownloadStatus::Done,
                Err(e) => {
                    s.state = DownloadStatus::Failed;
                    s.error = Some(e.to_string());
                }
            }
            s.finished_at = Some(now_millis());
        }
    });

    let snapshot = state.lock().unwrap_or_else(|e| e.into_inner()).clone();
    snapshot
}

/// Download the pinned Gemma 4 checkpoint by running `TurboFieldfareRepack`.
/// This does not copy the Hugging Face repo. The installer streams that
/// revision and writes `gemma4.gturbo` (or the vision sibling).
pub fn start_gturbo(local_models_dir: &Path, repack_bin: &Path, vision: bool) -> Result<DownloadState, super::gturbo::StartError> {
    let plan = super::gturbo::plan(local_models_dir, vision)?;
    let target_key = format!("gturbo:{}:{}", super::gturbo::REPO_ID, plan.file_label);
    {
        let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = reg.values().find(|h| {
            let current = h.state.lock().unwrap_or_else(|e| e.into_inner());
            !current.is_finished()
                && format!(
                    "{}:{}:{}",
                    current.format,
                    current.repo,
                    current.files.first().cloned().unwrap_or_default()
                ) == target_key
        }) {
            return Ok(existing.state.lock().unwrap_or_else(|e| e.into_inner()).clone());
        }
    }

    let download_id = uuid::Uuid::new_v4().to_string();
    let state = Arc::new(Mutex::new(DownloadState::new(&download_id, super::gturbo::REPO_ID, "gturbo")));
    {
        let mut current = state.lock().unwrap_or_else(|e| e.into_inner());
        current.files = vec![plan.file_label.clone()];
        current.total_bytes = plan.total_bytes;
        current.state = DownloadStatus::Downloading;
    }
    let cancel = CancellationToken::new();
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(download_id.clone(), Handle { state: Arc::clone(&state), cancel: cancel.clone() });

    let bin = repack_bin.to_path_buf();
    let tracked = Arc::clone(&state);
    tokio::spawn(async move {
        let outcome = super::gturbo::run_repack(&bin, &plan, &tracked, &cancel).await;
        let mut current = tracked.lock().unwrap_or_else(|e| e.into_inner());
        if current.state == DownloadStatus::Cancelled {
            current.finished_at = Some(now_millis());
            return;
        }
        match outcome {
            Ok(super::gturbo::RepackOutcome::Cancelled) => {
                current.state = DownloadStatus::Cancelled;
            }
            Ok(super::gturbo::RepackOutcome::Finished) => {
                current.state = DownloadStatus::Done;
                current.percent = Some(100.0);
                if current.total_bytes > 0 {
                    current.received_bytes = current.total_bytes;
                }
            }
            Err(e) => {
                current.state = DownloadStatus::Failed;
                current.error = Some(e.to_string());
            }
        }
        current.finished_at = Some(now_millis());
    });

    let snapshot = state.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(snapshot)
}

pub fn status(download_id: &str) -> Option<DownloadState> {
    registry().lock().unwrap_or_else(|e| e.into_inner()).get(download_id).map(|h| h.state.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

pub fn all() -> Vec<DownloadState> {
    registry().lock().unwrap_or_else(|e| e.into_inner()).values().map(|h| h.state.lock().unwrap_or_else(|e| e.into_inner()).clone()).collect()
}

pub fn cancel(download_id: &str) -> bool {
    match registry().lock().unwrap_or_else(|e| e.into_inner()).get(download_id) {
        Some(h) => {
            h.cancel.cancel();
            true
        }
        None => false,
    }
}

async fn run(
    repo: &str,
    revision: &str,
    format: &str,
    gguf_file: Option<String>,
    mmproj_file: Option<String>,
    local_models_dir: &Path,
    progress: Arc<Mutex<DownloadState>>,
    cancel: CancellationToken,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(120))
        .build()?;

    set(&progress, |s| s.state = DownloadStatus::Listing);
    let tree_url = format!("{HF_BASE}/api/models/{repo}/tree/{revision}?recursive=true");
    let tree: Vec<TreeEntry> = client.get(&tree_url).send().await?.error_for_status()?.json().await?;
    let all_files: Vec<TreeEntry> = tree.into_iter().filter(|e| e.entry_type == "file").collect();

    let wanted: Vec<&TreeEntry> = if format == "gguf" {
        let names: Vec<&str> = gguf_file.iter().chain(mmproj_file.iter()).map(String::as_str).collect();
        let found: Vec<&TreeEntry> = all_files.iter().filter(|e| names.iter().any(|n| e.path == *n || e.path.ends_with(&format!("/{n}")))).collect();
        if found.is_empty() {
            anyhow::bail!("none of {names:?} were found in `{repo}` at revision `{revision}`");
        }
        found
    } else {
        all_files.iter().filter(|e| !should_skip_for_mlx(&e.path)).collect()
    };
    if wanted.is_empty() {
        anyhow::bail!("`{repo}` has no downloadable files at revision `{revision}`");
    }

    let repo_dirname = repo.replace('/', "__");
    let dest_root = dest_dir(local_models_dir, format, &repo_dirname);
    let total: u64 = wanted.iter().map(|f| f.size).sum();
    set(&progress, |s| {
        s.files = wanted.iter().map(|f| f.path.clone()).collect();
        s.total_bytes = total;
        s.state = DownloadStatus::Downloading;
    });

    for entry in wanted {
        if cancel.is_cancelled() {
            set(&progress, |s| s.state = DownloadStatus::Cancelled);
            return Ok(());
        }
        let dst = resolve_download_dest(&dest_root, format, &entry.path)?;
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if let Ok(meta) = tokio::fs::metadata(&dst).await {
            if entry.size > 0 && meta.len() == entry.size {
                set(&progress, |s| s.received_bytes = s.received_bytes.saturating_add(entry.size));
                continue;
            }
        }

        let url = format!("{HF_BASE}/{repo}/resolve/{revision}/{}", entry.path);
        let resp = client.get(&url).send().await?.error_for_status()?;
        let mut stream = resp.bytes_stream();
        let part = dst.with_extension(format!("{}.part", dst.extension().and_then(|e| e.to_str()).unwrap_or("bin")));
        let mut file = tokio::fs::File::create(&part).await?;
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            if cancel.is_cancelled() {
                drop(file);
                let _ = tokio::fs::remove_file(&part).await;
                set(&progress, |s| s.state = DownloadStatus::Cancelled);
                return Ok(());
            }
            let bytes = chunk?;
            file.write_all(&bytes).await?;
            let n = bytes.len() as u64;
            set(&progress, |s| {
                s.received_bytes = s.received_bytes.saturating_add(n);
                s.percent = if s.total_bytes > 0 { Some((s.received_bytes as f64 / s.total_bytes as f64 * 100.0).min(100.0)) } else { None };
            });
        }
        file.flush().await?;
        drop(file);
        tokio::fs::rename(&part, &dst).await.context("rename part -> dest")?;
    }
    Ok(())
}

fn set(state: &Arc<Mutex<DownloadState>>, f: impl FnOnce(&mut DownloadState)) {
    f(&mut state.lock().unwrap_or_else(|e| e.into_inner()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mlx_skip_rules_match_the_old_downloader() {
        for skip in ["pytorch_model.bin", "model.onnx", "onnx/model.onnx", "weights.pth", ".gitattributes", "README.md", "model.gguf"] {
            assert!(should_skip_for_mlx(skip), "`{skip}` should be skipped");
        }
        for keep in ["config.json", "model.safetensors", "tokenizer.json"] {
            assert!(!should_skip_for_mlx(keep), "`{keep}` must be downloaded");
        }
    }

    #[test]
    fn dest_dir_separates_gguf_flat_from_mlx_repo_shape() {
        let root = Path::new("/models");
        assert_eq!(dest_dir(root, "gguf", "org__repo"), root.join("gguf").join("org__repo"));
        assert_eq!(dest_dir(root, "mlx", "org__repo"), root.join("org__repo"));
    }

    /// An MLX tree entry's path is joined onto the filesystem only after
    /// the same traversal guard the archive extractor uses — a GGUF entry
    /// never needs it, since only its basename is ever used.
    #[test]
    fn mlx_download_dest_rejects_a_traversal_path() {
        let root = Path::new("/models/org__repo");
        assert!(resolve_download_dest(root, "mlx", "../../etc/passwd").is_err());
        assert!(resolve_download_dest(root, "mlx", "/etc/passwd").is_err());
        let dst = resolve_download_dest(root, "mlx", "tokenizer/vocab.json").unwrap();
        assert_eq!(dst, root.join("tokenizer").join("vocab.json"));
    }

    #[test]
    fn gguf_download_dest_always_takes_the_basename_regardless_of_subdirectories() {
        let root = Path::new("/models/gguf/org__repo");
        let dst = resolve_download_dest(root, "gguf", "subdir/../../model.Q4_K_M.gguf").unwrap();
        assert_eq!(dst, root.join("model.Q4_K_M.gguf"));
    }

    /// `docs/runtime-protocol.md` §5.3 pins these exact strings on the wire.
    #[test]
    fn download_states_serialize_to_the_pinned_vocabulary() {
        for (state, text) in [
            (DownloadStatus::Queued, "\"queued\""),
            (DownloadStatus::Listing, "\"listing\""),
            (DownloadStatus::Downloading, "\"downloading\""),
            (DownloadStatus::Done, "\"done\""),
            (DownloadStatus::Failed, "\"failed\""),
            (DownloadStatus::Cancelled, "\"cancelled\""),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), text);
        }
    }

    #[test]
    fn a_finished_download_is_terminal() {
        let mut s = DownloadState::new("id", "a/b", "gguf");
        assert!(!s.is_finished());
        for st in [DownloadStatus::Done, DownloadStatus::Failed, DownloadStatus::Cancelled] {
            s.state = st;
            assert!(s.is_finished());
        }
    }

    /// The repack process can exit 0 and still have written the wrong
    /// checkpoint. That must not become a listed model. A second run that
    /// writes the pin does.
    #[tokio::test]
    async fn gturbo_download_keeps_only_the_pinned_checkpoint() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("models");
        std::fs::create_dir_all(&root).unwrap();
        let bin = tmp.path().join("repack.sh");

        let write_script = |good: bool| {
            let model = if good { crate::local_models::gturbo::REPO_ID } else { "other/model" };
            let hash = if good {
                format!("sha256:{}", crate::local_models::gturbo::SOURCE_INDEX_SHA256)
            } else {
                "sha256:dead".into()
            };
            let json = serde_json::json!({
                "magic": "GTURBO",
                "modelID": model,
                "sourceSnapshotHash": hash,
            });
            let mut script = String::from("#!/bin/sh\n");
            script.push_str("out=\"\"\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --output) out=\"$2\"; shift 2 ;;\n    *) shift ;;\n  esac\ndone\n");
            script.push_str("echo '[install] 7.3 GiB of 13.6 GiB (50%)' >&2\n");
            script.push_str("mkdir -p \"$out\"\ncat > \"$out/manifest.json\" <<'EOF'\n");
            script.push_str(&json.to_string());
            script.push_str("\nEOF\nprintf '%s' weights > \"$out/model_weights.bin\"\nexit 0\n");
            std::fs::write(&bin, script).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        };

        write_script(false);
        let bad = start_gturbo(&root, &bin, false).unwrap();
        let failed = wait_until_finished(&bad.download_id).await;
        assert_eq!(failed.state, DownloadStatus::Failed, "{:?}", failed.error);
        assert!(failed.error.as_deref().unwrap_or("").contains("pinned"), "{:?}", failed.error);
        assert!(crate::local_models::scan::scan_all(&root).is_empty());
        assert!(start_gturbo(&root, &bin, false).is_err(), "a foreign directory must not be overwritten");

        std::fs::remove_dir_all(root.join(crate::local_models::gturbo::TEXT_DIR_NAME)).unwrap();
        write_script(true);
        let good = start_gturbo(&root, &bin, false).unwrap();
        let done = wait_until_finished(&good.download_id).await;
        assert_eq!(done.state, DownloadStatus::Done, "{:?}", done.error);
        assert_eq!(done.repo, crate::local_models::gturbo::REPO_ID);
        let models = crate::local_models::scan::scan_all(&root);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].repo.as_deref(), Some(crate::local_models::gturbo::REPO_ID));
    }

    async fn wait_until_finished(id: &str) -> DownloadState {
        for _ in 0..100 {
            if let Some(state) = status(id) {
                if state.is_finished() {
                    return state;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("download {id} did not finish");
    }
}

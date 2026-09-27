//! `GET /api/local-models/hf-files?repo=` (`docs/runtime-protocol.md` §5.3) —
//! list a HuggingFace repo's files and classify them, so the UI can offer a
//! GGUF quant picker or confirm an MLX snapshot before spending a download.

use serde::{Deserialize, Serialize};

use super::keys::quant_from_filename;

const HF_BASE: &str = "https://huggingface.co";

#[derive(Debug, Clone, Serialize)]
pub struct HfFile {
    pub name: String,
    pub size: u64,
    /// The quant token for a `.gguf` file (`Q4_K_M`, …), `None` otherwise.
    pub quant: Option<String>,
    /// Whether this looks like a vision projector to pair with a GGUF model.
    pub mmproj: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HfFilesResponse {
    pub repo: String,
    pub format: &'static str,
    pub files: Vec<HfFile>,
}

#[derive(Deserialize)]
struct TreeEntry {
    #[serde(rename = "type")]
    entry_type: String,
    path: String,
    #[serde(default)]
    size: u64,
}

fn is_mmproj(name: &str) -> bool {
    name.to_ascii_lowercase().contains("mmproj")
}

/// `org/repo` from a bare id or a Hub URL — the same acceptance rule the
/// download endpoints use.
pub fn normalize_repo(raw: &str) -> Result<String, String> {
    let s = raw
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("huggingface.co/")
        .trim_start_matches("hf.co/")
        .trim_end_matches('/');
    let mut parts = s.split('/');
    let (Some(org), Some(name)) = (parts.next(), parts.next()) else {
        return Err(format!("expected a Hugging Face repo as `org/name`, got `{raw}`"));
    };
    let ok = |p: &str| !p.is_empty() && p != "." && p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok(org) || !ok(name) {
        return Err(format!("`{raw}` is not a valid `org/name` repo id"));
    }
    Ok(format!("{org}/{name}"))
}

fn classify(files: &[TreeEntry]) -> (&'static str, Vec<HfFile>) {
    let has_gguf = files.iter().any(|f| f.path.ends_with(".gguf"));
    let has_safetensors_and_config = files.iter().any(|f| f.path == "config.json") && files.iter().any(|f| f.path.ends_with(".safetensors"));
    let format = if has_gguf {
        "gguf"
    } else if has_safetensors_and_config {
        "mlx"
    } else {
        "unknown"
    };
    let out = files
        .iter()
        .filter(|f| format != "gguf" || f.path.ends_with(".gguf"))
        .map(|f| {
            let name = f.path.rsplit('/').next().unwrap_or(&f.path).to_string();
            let stem = name.strip_suffix(".gguf").unwrap_or(&name);
            HfFile {
                quant: name.ends_with(".gguf").then(|| quant_from_filename(stem)).flatten(),
                mmproj: is_mmproj(&name),
                name,
                size: f.size,
            }
        })
        .collect();
    (format, out)
}

pub async fn fetch(repo: &str, revision: &str) -> anyhow::Result<HfFilesResponse> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("{HF_BASE}/api/models/{repo}/tree/{revision}?recursive=true");
    let entries: Vec<TreeEntry> = client.get(&url).send().await?.error_for_status()?.json().await?;
    let files: Vec<TreeEntry> = entries.into_iter().filter(|e| e.entry_type == "file").collect();
    let (format, files) = classify(&files);
    Ok(HfFilesResponse { repo: repo.to_string(), format, files })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64) -> TreeEntry {
        TreeEntry { entry_type: "file".into(), path: path.into(), size }
    }

    #[test]
    fn repo_ids_come_from_ids_or_urls_and_reject_traversal() {
        assert_eq!(normalize_repo("org/repo").unwrap(), "org/repo");
        assert_eq!(normalize_repo("https://huggingface.co/org/repo/").unwrap(), "org/repo");
        for bad in ["repo", "../x", "a/..", "a b/c", ""] {
            assert!(normalize_repo(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_repo_with_gguf_files_is_classified_gguf_and_lists_quant_and_mmproj() {
        let files = vec![
            entry("model-Q4_K_M.gguf", 4_000_000_000),
            entry("model-Q8_0.gguf", 7_500_000_000),
            entry("mmproj-model-f16.gguf", 500_000_000),
            entry("README.md", 100),
        ];
        let (format, out) = classify(&files);
        assert_eq!(format, "gguf");
        assert_eq!(out.len(), 3, "README is not a gguf file and is excluded");
        let q4 = out.iter().find(|f| f.name.contains("Q4_K_M")).unwrap();
        assert_eq!(q4.quant.as_deref(), Some("Q4_K_M"));
        assert!(!q4.mmproj);
        let proj = out.iter().find(|f| f.mmproj).unwrap();
        assert!(proj.name.to_lowercase().contains("mmproj"));
    }

    #[test]
    fn a_repo_with_safetensors_and_config_is_classified_mlx() {
        let files = vec![entry("config.json", 100), entry("model.safetensors", 1_000_000_000), entry("tokenizer.json", 500)];
        let (format, out) = classify(&files);
        assert_eq!(format, "mlx");
        assert_eq!(out.len(), 3, "mlx classification lists every file, not just weights");
    }

    #[test]
    fn neither_shape_is_unknown() {
        let files = vec![entry("pytorch_model.bin", 100)];
        let (format, _) = classify(&files);
        assert_eq!(format, "unknown");
    }
}

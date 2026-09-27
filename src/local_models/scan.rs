//! Scan `~/.senclaw/local-models/` for MLX snapshots and GGUF files
//! (`docs/runtime-protocol.md` §5.3/§6.1). Nothing here downloads or loads a
//! model — this only says what is already on disk.

use std::path::{Path, PathBuf};

use sen_runtime_sdk::manifest::{Capability, ModelFormat, Slot};

use super::gguf;
use super::keys::{model_key, quant_from_filename};

/// Directories under the model root that are never listed as MLX/GGUF
/// checkpoints — the engine-private stores runtimes derive from `SENCLAW_HOME`
/// (§6.1: "the model library never lists engine-private folders").
const ENGINE_PRIVATE_DIRS: &[&str] = &["laya", "hf-cache", "whisper-models", "tts-models", "ocr-models", "gguf"];

#[derive(Debug, Clone)]
pub struct LocalModel {
    pub key: String,
    pub name: String,
    pub format: ModelFormat,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub capabilities: Vec<Capability>,
    pub vision: bool,
    pub embedding: bool,
    pub mmproj_path: Option<PathBuf>,
    pub quant: Option<String>,
    pub repo: Option<String>,
    pub context_length: Option<u32>,
}

impl LocalModel {
    pub fn capability_list(&self) -> Vec<Capability> {
        self.capabilities.clone()
    }

    pub fn slot(&self) -> Slot {
        Slot::for_format(self.format)
    }
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// `org/repo` from the on-disk `org__repo` directory name (the
/// `local-model-core` layout, unchanged by the split). `None` for a directory
/// this naming rule did not produce — a stray folder must not become a model.
fn dirname_to_repo(name: &str) -> Option<String> {
    if !name.contains("__") {
        return None;
    }
    let id = name.replacen("__", "/", 1);
    (id.split('/').count() == 2 && !id.starts_with('/') && !id.ends_with('/')).then_some(id)
}

/// Speech checkpoints use the same snapshot layout as language models
/// (`config.json` + safetensors) but belong to the speech-to-text runtime. Listed
/// here, one lands in the model picker as a chat model, and every turn sent to it
/// fails — the local-models root holds whisper snapshots on real machines.
const SPEECH_MODEL_TYPES: &[&str] = &["whisper"];

fn is_speech_checkpoint(cfg: &serde_json::Value) -> bool {
    let model_type = cfg.get("model_type").and_then(|v| v.as_str()).unwrap_or("");
    SPEECH_MODEL_TYPES.contains(&model_type)
        || cfg.get("architectures").and_then(|a| a.as_array()).is_some_and(|archs| {
            archs.iter().filter_map(|v| v.as_str()).any(|a| {
                a.contains("Whisper") || a.ends_with("ForCTC") || a.ends_with("ForSpeechSeq2Seq")
            })
        })
}

fn scan_mlx(root: &Path) -> Vec<LocalModel> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let dir = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !dir.is_dir() || ENGINE_PRIVATE_DIRS.contains(&name.as_str()) || !dir.join("config.json").exists() {
            continue;
        }
        let has_weights = std::fs::read_dir(&dir)
            .map(|it| it.filter_map(Result::ok).any(|e| e.file_name().to_string_lossy().ends_with(".safetensors")))
            .unwrap_or(false);
        if !has_weights {
            continue;
        }
        let repo = dirname_to_repo(&name);
        let cfg: serde_json::Value = std::fs::read_to_string(dir.join("config.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if is_speech_checkpoint(&cfg) {
            continue;
        }
        let vision = cfg.get("vision_config").is_some_and(|v| !v.is_null());
        let context_length = cfg
            .get("text_config")
            .and_then(|t| t.get("max_position_embeddings"))
            .and_then(|v| v.as_u64())
            .or_else(|| cfg.get("max_position_embeddings").and_then(|v| v.as_u64()))
            .map(|n| n as u32);
        let mut capabilities = vec![Capability::Chat];
        if vision {
            capabilities.push(Capability::Vision);
        }
        let rel = PathBuf::from(&name);
        out.push(LocalModel {
            key: model_key("mlx", &rel),
            name: repo.clone().unwrap_or_else(|| name.clone()),
            format: ModelFormat::Mlx,
            size_bytes: dir_size(&dir),
            capabilities,
            vision,
            embedding: false,
            mmproj_path: None,
            quant: None,
            repo,
            context_length,
            path: dir,
        });
    }
    out
}

/// `mmproj` files pair with a base model in the same directory — the
/// llama.cpp naming convention (`mmproj-<name>.gguf` beside `<name>.gguf`).
fn is_mmproj(name: &str) -> bool {
    name.to_ascii_lowercase().contains("mmproj")
}

fn scan_gguf(root: &Path) -> Vec<LocalModel> {
    let gguf_root = root.join("gguf");
    let mut out = Vec::new();
    let Ok(orgs) = std::fs::read_dir(&gguf_root) else {
        return out;
    };
    for org_entry in orgs.filter_map(Result::ok) {
        let org_dir = org_entry.path();
        if !org_dir.is_dir() {
            continue;
        }
        let Ok(listing) = std::fs::read_dir(&org_dir) else {
            continue;
        };
        let files: Vec<PathBuf> = listing
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("gguf"))
            .collect();
        let repo = dirname_to_repo(&org_entry.file_name().to_string_lossy());
        for file in &files {
            let file_name = file.file_name().unwrap().to_string_lossy().into_owned();
            if is_mmproj(&file_name) {
                continue; // paired below, never listed as its own model
            }
            let stem = file.file_stem().unwrap().to_string_lossy().into_owned();
            let mmproj = files.iter().find(|p| is_mmproj(&p.file_name().unwrap().to_string_lossy())).cloned();
            let meta = gguf::read_metadata(file);
            let context_length = meta.as_ref().and_then(|m| m.context_length());
            let is_embedding = meta.as_ref().is_some_and(|m| m.has_pooling_type());
            let mut capabilities = vec![if is_embedding { Capability::Embedding } else { Capability::Chat }];
            let vision = mmproj.is_some();
            if vision {
                capabilities.push(Capability::Vision);
            }
            let rel = file.strip_prefix(root).unwrap_or(file).to_path_buf();
            let size_bytes = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0)
                + mmproj.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()).unwrap_or(0);
            out.push(LocalModel {
                key: model_key("gguf", &rel),
                // The file stem, never the architecture (`general.architecture`
                // is "llama"/"qwen2"/… — every Llama or Qwen quant would list
                // identically and be unpickable in the model picker).
                name: stem.clone(),
                format: ModelFormat::Gguf,
                path: file.clone(),
                size_bytes,
                capabilities,
                vision,
                embedding: is_embedding,
                mmproj_path: mmproj,
                quant: quant_from_filename(&stem),
                repo: repo.clone(),
                context_length,
            });
        }
    }
    out
}

/// A finished TurboFieldfare text install: directory name `*.gturbo` (not the
/// `*.vision.gturbo` companion), `manifest.json` magic `GTURBO`, and the packed
/// weight file the runtime refuses to start without.
fn scan_gturbo(root: &Path) -> Vec<LocalModel> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let dir = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !dir.is_dir() || !is_gturbo_text_dir(&name) {
            continue;
        }
        let Some(manifest) = read_json_capped(&dir.join("manifest.json"), 4 * 1024 * 1024) else {
            continue;
        };
        if manifest.get("magic").and_then(|v| v.as_str()) != Some("GTURBO") {
            continue;
        }
        if !dir.join("model_weights.bin").is_file() {
            continue;
        }
        let model_id = manifest
            .get("modelID")
            .or_else(|| manifest.get("model_id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&name)
            .to_string();
        let stem = name.trim_end_matches(".gturbo");
        let vision_dir = dir.with_file_name(format!("{stem}.vision.gturbo"));
        let vision = vision_manifest_present(&vision_dir);
        let mut capabilities = vec![Capability::Chat];
        if vision {
            capabilities.push(Capability::Vision);
        }
        let rel = PathBuf::from(&name);
        out.push(LocalModel {
            key: model_key("gturbo", &rel),
            name: model_id,
            format: ModelFormat::Gturbo,
            path: dir.clone(),
            size_bytes: dir_size(&dir) + if vision { dir_size(&vision_dir) } else { 0 },
            capabilities,
            vision,
            embedding: false,
            mmproj_path: None,
            quant: Some("4-bit".into()),
            repo: None,
            context_length: Some(65_536),
        });
    }
    out
}

fn is_gturbo_text_dir(name: &str) -> bool {
    name.ends_with(".gturbo") && !name.ends_with(".vision.gturbo") && name.len() > ".gturbo".len()
}

fn vision_manifest_present(dir: &Path) -> bool {
    read_json_capped(&dir.join("manifest.json"), 4 * 1024 * 1024)
        .and_then(|v| v.get("magic").and_then(|m| m.as_str()).map(|s| s == "GTURBO-VISION"))
        .unwrap_or(false)
}

fn read_json_capped(path: &Path, max_bytes: u64) -> Option<serde_json::Value> {
    let len = std::fs::metadata(path).ok()?.len();
    if len > max_bytes {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Every local model on disk — MLX snapshots, GGUF files, and TurboFieldfare
/// `.gturbo` installs. Engine-private folders excluded. Pure filesystem work;
/// safe to call from a blocking task.
pub fn scan_all(local_models_dir: &Path) -> Vec<LocalModel> {
    let mut out = scan_mlx(local_models_dir);
    out.extend(scan_gguf(local_models_dir));
    out.extend(scan_gturbo(local_models_dir));
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.key.cmp(&b.key)));
    out
}

pub fn find_by_key(local_models_dir: &Path, key: &str) -> Option<LocalModel> {
    scan_all(local_models_dir).into_iter().find(|m| m.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_mlx_model(root: &Path, dirname: &str, vision: bool) {
        let dir = root.join(dirname);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = if vision {
            serde_json::json!({"max_position_embeddings": 8192, "vision_config": {"foo": 1}})
        } else {
            serde_json::json!({"max_position_embeddings": 4096})
        };
        std::fs::write(dir.join("config.json"), serde_json::to_string(&cfg).unwrap()).unwrap();
        std::fs::write(dir.join("model.safetensors"), b"weights").unwrap();
    }

    #[test]
    fn mlx_snapshot_is_scanned_with_repo_and_context_length() {
        let tmp = tempfile::tempdir().unwrap();
        write_mlx_model(tmp.path(), "mlx-community__gemma-4-e2b-it-4bit", false);
        let models = scan_all(tmp.path());
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].repo.as_deref(), Some("mlx-community/gemma-4-e2b-it-4bit"));
        assert_eq!(models[0].context_length, Some(4096));
        assert!(!models[0].vision);
    }

    #[test]
    fn mlx_vision_config_marks_the_capability() {
        let tmp = tempfile::tempdir().unwrap();
        write_mlx_model(tmp.path(), "org__vision-model", true);
        let models = scan_all(tmp.path());
        assert!(models[0].vision);
        assert!(models[0].capabilities.contains(&Capability::Vision));
    }

    #[test]
    fn engine_private_directories_are_never_listed() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ENGINE_PRIVATE_DIRS {
            if *name == "gguf" {
                continue; // scanned specially, exercised by the GGUF tests below
            }
            write_mlx_model(tmp.path(), name, false);
        }
        assert!(scan_all(tmp.path()).is_empty());
    }

    /// A whisper snapshot sits in the same root with the same layout (a real
    /// machine has `mlx-community__whisper-large-v3-turbo-4bit` there); it must
    /// not become a chat model in the picker.
    #[test]
    fn speech_checkpoints_are_not_listed_as_language_models() {
        let tmp = tempfile::tempdir().unwrap();
        for (dirname, cfg) in [
            ("mlx-community__whisper-large-v3-turbo-4bit", serde_json::json!({"model_type": "whisper", "n_mels": 128})),
            ("openai__whisper-small", serde_json::json!({"architectures": ["WhisperForConditionalGeneration"]})),
            ("facebook__wav2vec2-base", serde_json::json!({"architectures": ["Wav2Vec2ForCTC"]})),
        ] {
            let dir = tmp.path().join(dirname);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config.json"), serde_json::to_string(&cfg).unwrap()).unwrap();
            std::fs::write(dir.join("weights.safetensors"), b"weights").unwrap();
        }
        write_mlx_model(tmp.path(), "mlx-community__Qwen2.5-0.5B-Instruct-4bit", false);
        let models = scan_all(tmp.path());
        assert_eq!(models.len(), 1, "{:?}", models.iter().map(|m| &m.name).collect::<Vec<_>>());
        assert_eq!(models[0].repo.as_deref(), Some("mlx-community/Qwen2.5-0.5B-Instruct-4bit"));
    }

    #[test]
    fn a_config_without_weights_is_not_a_model() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("org__incomplete");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "{}").unwrap();
        assert!(scan_all(tmp.path()).is_empty());
    }

    fn write_gguf_file(path: &Path, arch: &str) {
        // Minimal real GGUF bytes (mirrors gguf::tests::fake_gguf) so the
        // scanner's own metadata read exercises the real parser end to end.
        let mut buf = Vec::new();
        buf.extend_from_slice(&0x4655_4747u32.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        let string_val = |s: &str| {
            let mut v = (s.len() as u64).to_le_bytes().to_vec();
            v.extend_from_slice(s.as_bytes());
            v
        };
        let kvs: Vec<(String, u32, Vec<u8>)> = vec![
            ("general.architecture".to_string(), 8, string_val(arch)),
            (format!("{arch}.context_length"), 4, 2048u32.to_le_bytes().to_vec()),
        ];
        buf.extend_from_slice(&(kvs.len() as u64).to_le_bytes());
        for (key, vt, val) in kvs {
            buf.extend_from_slice(&string_val(&key));
            buf.extend_from_slice(&vt.to_le_bytes());
            buf.extend_from_slice(&val);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, buf).unwrap();
    }

    #[test]
    fn gguf_model_and_its_mmproj_are_paired_and_the_mmproj_is_not_its_own_model() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gguf").join("org__repo");
        write_gguf_file(&dir.join("model-Q4_K_M.gguf"), "llama");
        write_gguf_file(&dir.join("mmproj-model-f16.gguf"), "clip");

        let models = scan_all(tmp.path());
        assert_eq!(models.len(), 1, "the mmproj file must not appear as its own model");
        let m = &models[0];
        assert_eq!(m.format, ModelFormat::Gguf);
        assert_eq!(m.name, "model-Q4_K_M", "the file stem, never `general.architecture` (\"llama\") — every quant would list identically");
        assert!(m.vision, "presence of a paired mmproj marks vision");
        assert!(m.mmproj_path.is_some());
        assert_eq!(m.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(m.repo.as_deref(), Some("org/repo"));
        assert_eq!(m.context_length, Some(2048));

        let found = find_by_key(tmp.path(), &m.key).unwrap();
        assert_eq!(found.path, m.path);
    }

    #[test]
    fn gturbo_install_is_listed_and_a_vision_pack_is_not_its_own_model() {
        let tmp = tempfile::tempdir().unwrap();
        let text = tmp.path().join("gemma4.gturbo");
        std::fs::create_dir_all(&text).unwrap();
        std::fs::write(
            text.join("manifest.json"),
            r#"{"magic":"GTURBO","modelID":"gemma-4-26b-a4b-it"}"#,
        )
        .unwrap();
        std::fs::write(text.join("model_weights.bin"), b"weights").unwrap();
        let vision = tmp.path().join("gemma4.vision.gturbo");
        std::fs::create_dir_all(&vision).unwrap();
        std::fs::write(vision.join("manifest.json"), r#"{"magic":"GTURBO-VISION"}"#).unwrap();

        let models = scan_all(tmp.path());
        assert_eq!(models.len(), 1, "{:?}", models.iter().map(|m| &m.name).collect::<Vec<_>>());
        assert_eq!(models[0].format, ModelFormat::Gturbo);
        assert_eq!(models[0].name, "gemma-4-26b-a4b-it");
        assert!(models[0].vision);
        assert_eq!(models[0].slot(), Slot::Gturbo);
        assert!(models[0].key.starts_with("gturbo-"));
    }

    #[test]
    fn an_incomplete_gturbo_directory_is_not_a_model() {
        let tmp = tempfile::tempdir().unwrap();
        let text = tmp.path().join("partial.gturbo");
        std::fs::create_dir_all(&text).unwrap();
        std::fs::write(text.join("manifest.json"), r#"{"magic":"GTURBO","modelID":"x"}"#).unwrap();
        assert!(scan_all(tmp.path()).is_empty());
    }
}

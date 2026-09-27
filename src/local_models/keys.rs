//! Stable model keys: `<format>-<slug>-<8 hex of sha256(format + relative path)>`
//! (`docs/runtime-protocol.md` §5.3). Stable across a rescan, URL-safe, and
//! traceable back to a human-readable name.

use sha2::{Digest, Sha256};
use std::path::Path;

/// Lowercase alphanumerics separated by single hyphens; never empty.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "model".to_string()
    } else {
        out
    }
}

/// The quant token out of a GGUF filename (`Q4_K_M`, `Q8_0`, `IQ4_XS`, `F16`,
/// …) — llama.cpp's own naming convention. Split only on `-`/`.`, never `_`:
/// the token itself routinely *contains* underscores (`Q4_K_M` is one quant,
/// not three), so splitting on it would truncate `Q4_K_M` down to `Q4`. Read
/// from the right so a repo name that happens to contain a similar-looking
/// substring earlier does not match instead.
pub fn quant_from_filename(stem: &str) -> Option<String> {
    stem.split(['-', '.'])
        .rev()
        .find(|t| {
            let up = t.to_ascii_uppercase();
            let after_q = up.strip_prefix("IQ").or_else(|| up.strip_prefix('Q'));
            // A digit right after `Q`/`IQ` is what actually distinguishes a
            // quant token (`Q4_K_M`, `IQ4_XS`) from an ordinary word that
            // happens to start with the same letter (a repo or file segment
            // named "quant", say).
            after_q.is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit())) || matches!(up.as_str(), "F16" | "F32" | "BF16")
        })
        .map(str::to_string)
}

/// `relative_path` is relative to the local-models root (the MLX snapshot
/// directory name, or the GGUF file's `gguf/<org>__<repo>/<file>.gguf` path) —
/// hashed together with `format` so an MLX directory and a GGUF file that
/// happen to share a name never collide.
pub fn model_key(format: &str, relative_path: &Path) -> String {
    let rel = relative_path.to_string_lossy().replace('\\', "/");
    let mut hasher = Sha256::new();
    hasher.update(format.as_bytes());
    hasher.update(b"\0");
    hasher.update(rel.as_bytes());
    let digest = hasher.finalize();
    let hex8 = hex::encode(&digest[..4]);
    // A GGUF key names the file without its `.gguf`; an MLX key names the whole
    // snapshot directory. `file_stem` on a directory treats everything after the
    // last dot as an extension, so `mlx-community__Qwen2.5-0.5B-Instruct-4bit`
    // would come out as `qwen2-5-0` — a key that no longer says which model it is.
    let base = if relative_path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gguf")) {
        relative_path.file_stem()
    } else {
        relative_path.file_name()
    };
    let slug = slugify(base.and_then(|s| s.to_str()).unwrap_or("model"));
    format!("{format}-{slug}-{hex8}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_lowercases_and_collapses_separators() {
        assert_eq!(slugify("Qwen2.5-7B-Instruct"), "qwen2-5-7b-instruct");
        assert_eq!(slugify("mlx-community__gemma"), "mlx-community-gemma");
        assert_eq!(slugify("___"), "model");
        assert_eq!(slugify(""), "model");
    }

    #[test]
    fn model_key_is_stable_and_url_safe() {
        let a = model_key("gguf", Path::new("gguf/org__repo/model-Q4_K_M.gguf"));
        let b = model_key("gguf", Path::new("gguf/org__repo/model-Q4_K_M.gguf"));
        assert_eq!(a, b, "must be stable across rescans");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "{a}");
        assert!(a.starts_with("gguf-model-q4-k-m-"), "{a}");
    }

    /// An MLX snapshot directory name is not a file name: its dots are part of
    /// the model name (`Qwen2.5-0.5B`), not an extension.
    #[test]
    fn mlx_key_keeps_the_whole_directory_name() {
        let key = model_key("mlx", Path::new("mlx-community__Qwen2.5-0.5B-Instruct-4bit"));
        assert!(key.starts_with("mlx-mlx-community-qwen2-5-0-5b-instruct-4bit-"), "{key}");
        let gguf = model_key("gguf", Path::new("gguf/Qwen__Qwen2.5-0.5B-Instruct-GGUF/qwen2.5-0.5b-instruct-q4_0.gguf"));
        assert!(gguf.starts_with("gguf-qwen2-5-0-5b-instruct-q4-0-"), "{gguf}");
    }

    #[test]
    fn different_formats_never_collide_on_the_same_name() {
        let rel = Path::new("same-name");
        assert_ne!(model_key("gguf", rel), model_key("mlx", rel));
    }

    /// A quant token routinely *contains* underscores (`Q4_K_M` is one quant,
    /// not three) — splitting on `_` would truncate it to `Q4`.
    #[test]
    fn quant_token_is_not_truncated_at_its_own_underscores() {
        assert_eq!(quant_from_filename("model-Q4_K_M"), Some("Q4_K_M".to_string()));
        assert_eq!(quant_from_filename("model.Q8_0"), Some("Q8_0".to_string()));
        assert_eq!(quant_from_filename("model-IQ4_XS"), Some("IQ4_XS".to_string()));
        assert_eq!(quant_from_filename("mmproj-model-f16"), Some("f16".to_string()));
        assert_eq!(quant_from_filename("model-no-quant-here"), None);
    }
}

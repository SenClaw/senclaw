//! Embedding model introspection for Settings → Embedding.
//!
//! Before the runtime split this listed a curated catalog of HuggingFace
//! sentence-transformer checkpoints for the in-process candle backend. The
//! daemon links no inference code now: "local" embeddings run on a GGUF
//! checkpoint through the same `gguf` runtime chat models use
//! (`docs/runtime-protocol.md` §5.5), so this lists whichever installed local
//! models can actually serve them instead of a fixed download list.
//!
//! **Shape change from the pre-split daemon** (documented, not silently
//! swapped): `GET /api/embedding/features` no longer reports `candle`/
//! `candle_metal`/`mlx_static` — it reports `local` (an embedding-capable
//! model plus a `gguf` runtime are both present) and `modelsDir`.
//! `GET /api/embedding/models` returns installed local models with the
//! `embedding` capability instead of a HuggingFace catalog with `installed`
//! flags. `POST /api/embedding/download-model` no longer downloads anything
//! here — a local embedding model is fetched like any other GGUF model via
//! `POST /api/local-models/download`, so this route answers 410 Gone naming
//! the replacement.

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_json::json;

use sen_runtime_sdk::manifest::{Capability, Slot};

use super::core::UiState;

/// `GET /api/embedding/features`.
pub(crate) async fn embedding_features(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let models = crate::local_models::scan_all(&s.config.paths.local_models_dir);
    let has_embedding_model = models.iter().any(|m| m.capabilities.contains(&Capability::Embedding));
    let gguf_runtime_installed = s
        .runtime_manager
        .as_ref()
        .is_some_and(|mgr| mgr.installed().iter().any(|p| p.manifest.slots.contains(&Slot::Gguf)));
    Json(json!({
        "local": has_embedding_model && gguf_runtime_installed,
        "modelsDir": s.config.paths.local_models_dir,
    }))
}

/// `GET /api/embedding/models` — installed local models that can serve
/// embeddings, in the same key/name shape `/api/local-models` uses.
pub(crate) async fn embedding_list_models(State(s): State<Arc<UiState>>) -> Json<serde_json::Value> {
    let models: Vec<serde_json::Value> = crate::local_models::scan_all(&s.config.paths.local_models_dir)
        .into_iter()
        .filter(|m| m.capabilities.contains(&Capability::Embedding))
        .map(|m| {
            json!({
                "key": m.key, "name": m.name, "repo": m.repo, "quant": m.quant,
                "sizeBytes": m.size_bytes, "contextLength": m.context_length,
            })
        })
        .collect();
    Json(json!({ "models": models }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct DownloadBody {
    #[allow(dead_code)]
    pub model: String,
}

/// `POST /api/embedding/download-model` — retired in favor of
/// `POST /api/local-models/download`, which downloads any GGUF model
/// (embedding included) into the shared local-model library.
pub(crate) async fn embedding_download_model(Json(_body): Json<DownloadBody>) -> Response {
    (
        StatusCode::GONE,
        Json(json!({
            "error": "downloading a local embedding model moved to POST /api/local-models/download \
                      (pass the GGUF file you want, e.g. an embedding-tagged quant)",
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_body_still_parses_the_old_shape() {
        let body: DownloadBody = serde_json::from_str(r#"{"model": "bge-small-en-v1.5"}"#).unwrap();
        assert_eq!(body.model, "bge-small-en-v1.5");
    }
}

//! Embedding provider implementations. Mirrors `src-old/memory/embedding-providers.ts`.
//!
//! Four providers: OpenAI (batch of 8), OpenRouter (single), Ollama (single),
//! Local (pure-Rust candle/BERT — enable with `--features local-embed`).

use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tokio::time::sleep;

use super::embedding::EmbeddingProvider;

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: u32 = 3;

fn jitter_ms(attempt: u32) -> Duration {
    // rand::random is Send-safe (doesn't hold Rng across calls)
    let base = 1000u64 * 2u64.pow(attempt);
    let jitter = rand::random::<u64>() % 1000;
    Duration::from_millis(base + jitter)
}

fn build_client() -> Result<Client> {
    Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .context("build reqwest client")
}

// ===== OpenAI =====

pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    base_url: String,
}

impl OpenAiProvider {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self {
            client: build_client().expect("reqwest"),
            api_key,
            base_url,
        }
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        "text-embedding-3-small"
    }

    fn dimensions(&self) -> u32 {
        1536
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut all: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for batch in texts.chunks(8) {
            let result = Self::call_api(&self.client, &self.api_key, &self.base_url, batch).await?;
            all.extend(result);
        }
        Ok(all)
    }
}

#[derive(Deserialize)]
struct OpenAiResponse {
    data: Vec<OpenAiEmbeddingData>,
    /// `{"prompt_tokens": N, "total_tokens": N}` — present on OpenAI and
    /// OpenRouter, absent on most local servers.
    #[serde(default)]
    usage: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

/// Feed one embedding call into the global usage recorder (source
/// `embedding`; output tokens are always 0). No-op when the provider sent no
/// usage object or the daemon recorder isn't running.
fn record_embedding_usage(provider: &str, model: &str, usage_json: Option<&serde_json::Value>) {
    let Some(rec) = crate::usage::global() else {
        return;
    };
    let Some(v) = usage_json else {
        return;
    };
    let Some(u) = crate::zen_core::RawUsage::from_json(v) else {
        return;
    };
    rec.record(
        crate::usage::UsageEvent {
            agent_id: "embedding".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            ..crate::usage::UsageEvent::new(crate::usage::UsageSource::Embedding)
        }
        .with_tokens(&u),
    );
}

impl OpenAiProvider {
    async fn call_api(
        client: &Client,
        api_key: &str,
        base_url: &str,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/embeddings", base_url.trim_end_matches('/'));
        for attempt in 0..MAX_RETRIES {
            let res = client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&serde_json::json!({
                    "model": "text-embedding-3-small",
                    "input": texts,
                }))
                .send()
                .await;

            match res {
                Ok(r) if r.status().is_success() => {
                    let body: OpenAiResponse =
                        r.json().await.context("parse OpenAI embedding response")?;
                    record_embedding_usage(
                        "openai",
                        "text-embedding-3-small",
                        body.usage.as_ref(),
                    );
                    let mut sorted = body.data;
                    sorted.sort_by_key(|d| d.index);
                    return Ok(sorted.into_iter().map(|d| d.embedding).collect());
                }
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    if attempt < MAX_RETRIES - 1 {
                        sleep(jitter_ms(attempt)).await;
                        continue;
                    }
                    bail!("OpenAI API {status}: {body}");
                }
                Err(e) => {
                    if attempt < MAX_RETRIES - 1 {
                        sleep(jitter_ms(attempt)).await;
                        continue;
                    }
                    return Err(e.into());
                }
            }
        }
        bail!("embedding failed after {MAX_RETRIES} retries");
    }
}

// ===== OpenRouter =====

pub struct OpenRouterProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
    dims: std::sync::Mutex<u32>,
}

impl OpenRouterProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            client: build_client().expect("reqwest"),
            api_key,
            base_url,
            model,
            dims: std::sync::Mutex::new(1536),
        }
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for OpenRouterProvider {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> u32 {
        *self.dims.lock().unwrap()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            let vec =
                Self::embed_single(&self.client, &self.api_key, &self.base_url, &self.model, t)
                    .await?;
            if out.is_empty() && !vec.is_empty() {
                *self.dims.lock().unwrap() = vec.len() as u32;
            }
            out.push(vec);
        }
        Ok(out)
    }
}

impl OpenRouterProvider {
    async fn embed_single(
        client: &Client,
        api_key: &str,
        base_url: &str,
        model: &str,
        text: &str,
    ) -> Result<Vec<f32>> {
        let url = format!("{}/embeddings", base_url.trim_end_matches('/'));
        for attempt in 0..MAX_RETRIES {
            let res = client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&serde_json::json!({ "model": model, "input": text }))
                .send()
                .await;

            match res {
                Ok(r) if r.status().is_success() => {
                    let body: OpenAiResponse = r
                        .json()
                        .await
                        .context("parse OpenRouter embedding response")?;
                    record_embedding_usage("openrouter", model, body.usage.as_ref());
                    return Ok(body
                        .data
                        .first()
                        .map(|d| d.embedding.clone())
                        .unwrap_or_default());
                }
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    if attempt < MAX_RETRIES - 1 {
                        sleep(jitter_ms(attempt)).await;
                        continue;
                    }
                    bail!("OpenRouter API {status}: {body}");
                }
                Err(e) => {
                    if attempt < MAX_RETRIES - 1 {
                        sleep(jitter_ms(attempt)).await;
                        continue;
                    }
                    return Err(e.into());
                }
            }
        }
        bail!("embedding failed after {MAX_RETRIES} retries");
    }
}

// ===== Ollama =====

pub struct OllamaProvider {
    client: Client,
    base_url: String,
    model: String,
    dims: std::sync::Mutex<u32>,
}

impl OllamaProvider {
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            client: build_client().expect("reqwest"),
            base_url,
            model: normalize_ollama_model(&model),
            dims: std::sync::Mutex::new(1536),
        }
    }
}

#[derive(Deserialize)]
struct OllamaResponse {
    embedding: Option<Vec<f32>>,
}

#[async_trait::async_trait]
impl EmbeddingProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> u32 {
        *self.dims.lock().unwrap()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            let url = format!("{}/api/embeddings", self.base_url.trim_end_matches('/'));
            let res = self
                .client
                .post(&url)
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({ "model": self.model, "prompt": t }))
                .send()
                .await
                .context("ollama embeddings request")?;

            if !res.status().is_success() {
                let status = res.status();
                let body = res.text().await.unwrap_or_default();
                bail!("ollama embeddings failed: {status} {body}");
            }

            let data: OllamaResponse = res.json().await.context("parse ollama response")?;
            let vec = data.embedding.unwrap_or_default();
            if out.is_empty() && !vec.is_empty() {
                *self.dims.lock().unwrap() = vec.len() as u32;
            }
            out.push(vec);
        }
        Ok(out)
    }
}

fn normalize_ollama_model(model: &str) -> String {
    let t = model.trim();
    if t.is_empty() {
        return "nomic-embed-text".into();
    }
    if let Some(stripped) = t.strip_prefix("ollama/") {
        return stripped.to_owned();
    }
    if regex::Regex::new(r"(?i)^(text-embedding-3|text-embedding-ada|embedding.*openai)")
        .unwrap()
        .is_match(t)
    {
        return "nomic-embed-text".into();
    }
    t.to_owned()
}

// ===== Local (GGUF embedding model via the runtime model route) =====
//
// The daemon links no inference code: a "local" embedding model is a GGUF
// checkpoint with the `embedding` capability, served by whichever runtime
// fills the `gguf` slot (llama.cpp) and reached the same way a `local:<key>`
// chat model is — the OpenAI-compatible route the daemon proxies to that
// model's process (docs/runtime-protocol.md §5.4-§5.5). `model` is the local
// model's stable key (`crate::local_models::keys::model_key`), never a
// HuggingFace repo name — that lookup moved to the runtime's own model
// library.

pub struct LocalProvider {
    client: Client,
    model: String,
    ui_port: u16,
    /// Seeded from a name-based guess, replaced with the checkpoint's real
    /// output size once the first response reports it - the same pattern
    /// `OllamaProvider` uses.
    dims: std::sync::Mutex<u32>,
}

impl LocalProvider {
    pub fn new(model: Option<String>, ui_port: u16) -> Self {
        let model = model.unwrap_or_default();
        // `/api/embedding-config`'s `modelName` is the bare model key; a
        // `local:` prefix is accepted and stripped (docs/runtime-protocol.md
        // §5.5) — a client (desktop has done this) may still send the LLM
        // config id shape instead of the bare key the runtime model route
        // expects.
        let model = model.strip_prefix(crate::local_models::ID_PREFIX).map(str::to_string).unwrap_or(model);
        let dims = local_dims_hint(&model);
        Self {
            client: build_client().expect("reqwest"),
            model,
            ui_port,
            dims: std::sync::Mutex::new(dims),
        }
    }
}

/// large -> 1024, base -> 768, everything else (small/MiniLM) -> 384. The
/// model key still carries the checkpoint's slug (`crate::local_models::keys`),
/// so this keeps working as a best-effort guess before the first real answer.
fn local_dims_hint(model: &str) -> u32 {
    let m = model.to_lowercase();
    if m.contains("large") {
        1024
    } else if m.contains("base") {
        768
    } else {
        384
    }
}

#[derive(Deserialize)]
struct LocalEmbeddingsResponse {
    data: Vec<LocalEmbeddingDatum>,
}

#[derive(Deserialize)]
struct LocalEmbeddingDatum {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

#[async_trait::async_trait]
impl EmbeddingProvider for LocalProvider {
    fn name(&self) -> &str {
        "local"
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn dimensions(&self) -> u32 {
        *self.dims.lock().unwrap()
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if self.model.trim().is_empty() {
            bail!(
                "no local embedding model selected - pick an installed GGUF embedding \
                 model in Settings -> Embedding"
            );
        }
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let url = format!(
            "http://127.0.0.1:{}/api/runtimes/models/{}/v1/embeddings",
            self.ui_port, self.model
        );
        let mut req = self.client.post(&url).json(&serde_json::json!({
            "model": self.model,
            "input": texts,
        }));
        // A loopback call into the daemon's own API needs the token too once
        // `SENCLAW_AUTH_MODE=always` is in force.
        if let Some((name, value)) = crate::util::internal_auth::header_for(&url) {
            req = req.header(name, value);
        }
        let resp = req.send().await.context("local embeddings request")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("local embeddings failed: {status} {body}");
        }
        let mut parsed: LocalEmbeddingsResponse =
            resp.json().await.context("parse local embeddings response")?;
        parsed.data.sort_by_key(|d| d.index);
        if let Some(first) = parsed.data.first() {
            *self.dims.lock().unwrap() = first.embedding.len() as u32;
        }
        Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── dims heuristic ────────────────────────────────────────────────────────

    #[test]
    fn dims_hint_large() {
        assert_eq!(local_dims_hint("bge-large-en-v1.5"), 1024);
        assert_eq!(local_dims_hint("multilingual-e5-large"), 1024);
    }

    #[test]
    fn dims_hint_base() {
        assert_eq!(local_dims_hint("bge-base-en-v1.5"), 768);
        assert_eq!(local_dims_hint("multilingual-e5-base"), 768);
    }

    #[test]
    fn dims_hint_small_and_minilm() {
        assert_eq!(local_dims_hint("all-MiniLM-L6-v2"), 384);
        assert_eq!(local_dims_hint("all-MiniLM-L12-v2"), 384);
        assert_eq!(
            local_dims_hint("paraphrase-multilingual-MiniLM-L12-v2"),
            384
        );
        assert_eq!(local_dims_hint("multilingual-e5-small"), 384);
    }

    // ── LocalProvider metadata (no network) ───────────────────────────────────

    #[test]
    fn local_provider_defaults() {
        let p = LocalProvider::new(None, 18788);
        assert_eq!(p.name(), "local");
        assert_eq!(p.model(), "");
        assert_eq!(p.dimensions(), 384);
    }

    #[test]
    fn local_provider_custom_model() {
        let p = LocalProvider::new(Some("gguf-bge-large-en-v1-5-a1b2c3d4".into()), 18788);
        assert_eq!(p.model(), "gguf-bge-large-en-v1-5-a1b2c3d4");
        assert_eq!(p.dimensions(), 1024);
    }

    /// `docs/runtime-protocol.md` §5.5: "a leading `local:` is accepted and
    /// stripped" — desktop has sent `modelName: "local:<key>"`, which 404s
    /// against `/api/runtimes/models/local:<key>/...` if not stripped here.
    #[test]
    fn local_provider_strips_a_leading_local_prefix() {
        let p = LocalProvider::new(Some("local:gguf-bge-large-en-v1-5-a1b2c3d4".into()), 18788);
        assert_eq!(p.model(), "gguf-bge-large-en-v1-5-a1b2c3d4");
        assert_eq!(p.dimensions(), 1024, "the stripped key must still drive the size hint");
    }

    #[tokio::test]
    async fn embed_without_a_selected_model_is_a_clean_error() {
        let p = LocalProvider::new(None, 18788);
        let err = p.embed(&["hi".to_string()]).await.unwrap_err();
        assert!(err.to_string().contains("no local embedding model selected"), "{err}");
    }

    #[tokio::test]
    async fn embed_of_an_empty_batch_never_calls_out() {
        // ui_port 0 would fail to connect if this actually dialed anything —
        // an empty batch must short-circuit before that.
        let p = LocalProvider::new(Some("gguf-nomic-embed-text-a1b2c3d4".into()), 0);
        let result = p.embed(&[] as &[String]).await.unwrap();
        assert!(result.is_empty());
    }

    // ── Ollama model name normalisation ───────────────────────────────────────

    #[test]
    fn ollama_strips_prefix() {
        let p = OllamaProvider::new(
            "http://localhost:11434".into(),
            "ollama/nomic-embed-text".into(),
        );
        assert_eq!(p.model(), "nomic-embed-text");
    }

    #[test]
    fn ollama_maps_openai_model_to_nomic() {
        let p = OllamaProvider::new(
            "http://localhost:11434".into(),
            "text-embedding-3-small".into(),
        );
        assert_eq!(p.model(), "nomic-embed-text");
    }

    #[test]
    fn ollama_empty_defaults_to_nomic() {
        let p = OllamaProvider::new("http://localhost:11434".into(), "".into());
        assert_eq!(p.model(), "nomic-embed-text");
    }
}

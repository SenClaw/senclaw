//! OCR MCP server. Exposes on-device PaddleOCR via two tools:
//!   - `ocr_recognize(image_path, language?)` — single image → recognized text + blocks
//!   - `ocr_batch(dir, glob?, language?)`     — every matching image in a folder
//!
//! The subprocess does NOT link MNN itself — it forwards the image bytes to the
//! main daemon's `/api/ocr/recognize` endpoint, which holds the engine cache.
//! This keeps the MCP binary cheap and avoids duplicating the C++ toolchain.

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use serde::Serialize;
use std::path::PathBuf;

use crate::mcp::schedule_server::ToolResult;

// ── Parameter schemas ────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct OcrRecognizeParams {
    /// Absolute or workspace-relative path to an image file
    /// (png/jpg/jpeg/webp/bmp/gif).
    image_path: String,
    /// IETF language code hint (e.g. "vi", "en", "zh"). Defaults to the
    /// system-wide OCR language saved in Settings.
    #[serde(default)]
    language: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct OcrBatchParams {
    /// Directory to scan.
    dir: String,
    /// Optional glob pattern (e.g. "*.png"). Defaults to all common image
    /// extensions.
    #[serde(default)]
    glob: Option<String>,
    /// Optional language hint forwarded to every recognition call.
    #[serde(default)]
    language: Option<String>,
}

// ── MCP server ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct McpOcrServer {
    bridge_base: String,
}

impl McpOcrServer {
    /// Build from `SENCLAW_OCR_BRIDGE_URL`, or `None` when it is absent. See
    /// [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        Ok(std::env::var("SENCLAW_OCR_BRIDGE_URL")
            .ok()
            .map(|bridge_base| Self { bridge_base }))
    }

    fn inner(&self) -> OcrBridge {
        OcrBridge::new(&self.bridge_base)
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpOcrServer {
    #[rmcp::tool(
        description = "Extract text from an image file using on-device OCR (PaddleOCR + MNN). Returns recognized text and per-block bounding boxes."
    )]
    fn ocr_recognize(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            OcrRecognizeParams,
        >,
    ) -> String {
        self.inner()
            .recognize_path(&p.image_path, p.language.as_deref())
            .content
    }

    #[rmcp::tool(
        description = "Run OCR on every image in a directory (png/jpg/jpeg/webp/bmp/gif). Returns one block per file with recognized text."
    )]
    fn ocr_batch(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            OcrBatchParams,
        >,
    ) -> String {
        self.inner()
            .batch(&p.dir, p.glob.as_deref(), p.language.as_deref())
            .content
    }
}

pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpOcrServer::from_env()?.context("SENCLAW_OCR_BRIDGE_URL not set")?;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// ── Bridge to the daemon's /api/ocr/recognize endpoint ───────────────────────

struct OcrBridge {
    base: String,
}

#[derive(Debug, Serialize)]
struct FileOcr {
    path: String,
    text: String,
    block_count: usize,
}

impl OcrBridge {
    fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
        }
    }

    fn recognize_path(&self, image_path: &str, language: Option<&str>) -> ToolResult {
        let path = PathBuf::from(image_path);
        if !path.exists() {
            return ToolResult::err(format!("image not found: {}", path.display()));
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return ToolResult::err(format!("read {}: {e}", path.display())),
        };
        let filename = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "image.bin".to_string());
        match self.post_image(filename, bytes, language) {
            Ok(json) => {
                let text = json
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let body = serde_json::json!({
                    "path": path.to_string_lossy(),
                    "text": text,
                    "blocks": json.get("blocks").cloned().unwrap_or(serde_json::Value::Null),
                });
                ToolResult::ok(serde_json::to_string_pretty(&body).unwrap_or_default())
            }
            Err(e) => ToolResult::err(format!("OCR request failed: {e}")),
        }
    }

    fn batch(&self, dir: &str, glob_pat: Option<&str>, language: Option<&str>) -> ToolResult {
        let dir_path = PathBuf::from(dir);
        if !dir_path.is_dir() {
            return ToolResult::err(format!("not a directory: {}", dir_path.display()));
        }
        let pat_string = glob_pat.map(|p| format!("{}/{}", dir, p));
        let patterns: Vec<String> = if let Some(p) = pat_string {
            vec![p]
        } else {
            ["png", "jpg", "jpeg", "webp", "bmp", "gif"]
                .iter()
                .map(|ext| format!("{}/**/*.{}", dir, ext))
                .collect()
        };
        let mut paths: Vec<PathBuf> = Vec::new();
        for pat in &patterns {
            if let Ok(iter) = glob::glob(pat) {
                for entry in iter.flatten() {
                    if entry.is_file() {
                        paths.push(entry);
                    }
                }
            }
        }
        paths.sort();
        paths.dedup();

        let mut results = Vec::new();
        let mut errors = Vec::new();
        for p in paths.iter().take(64) {
            let filename = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "image.bin".to_string());
            let bytes = match std::fs::read(p) {
                Ok(b) => b,
                Err(e) => {
                    errors.push(format!("{}: {e}", p.display()));
                    continue;
                }
            };
            match self.post_image(filename, bytes, language) {
                Ok(json) => {
                    let text = json
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let block_count = json
                        .get("blocks")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    results.push(FileOcr {
                        path: p.to_string_lossy().to_string(),
                        text,
                        block_count,
                    });
                }
                Err(e) => errors.push(format!("{}: {e}", p.display())),
            }
        }

        let body = serde_json::json!({
            "count": results.len(),
            "files": results,
            "errors": errors,
            "truncated": paths.len() > 64,
        });
        ToolResult::ok(serde_json::to_string_pretty(&body).unwrap_or_default())
    }

    fn post_image(
        &self,
        filename: String,
        bytes: Vec<u8>,
        language: Option<&str>,
    ) -> Result<serde_json::Value> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        let mut form = reqwest::blocking::multipart::Form::new().part(
            "image",
            reqwest::blocking::multipart::Part::bytes(bytes).file_name(filename),
        );
        if let Some(lang) = language {
            form = form.text("language", lang.to_string());
        }
        let resp = client
            .post(format!("{}/api/ocr/recognize", self.base))
            .multipart(form)
            .send()?
            .error_for_status()?;
        let json: serde_json::Value = resp.json()?;
        Ok(json)
    }
}

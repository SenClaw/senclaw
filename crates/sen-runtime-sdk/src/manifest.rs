//! `senclaw-runtime.json` — what a runtime package is and how to launch it.
//!
//! The manifest sits at the root of an installed package
//! (`~/.senclaw/runtimes/<id>/<version>/senclaw-runtime.json`) and is written
//! **last** by every installer: a directory without it is an interrupted
//! install, never a runtime.
//!
//! Parsing is deliberately strict where a mistake would be silent. A misspelt
//! enum value (`"slots": ["ggfu"]`) or placeholder (`"{prot}"`) is an error,
//! not a runtime that installs and is never selected, or a process started on
//! the literal port `{prot}`. Unknown *keys* are tolerated for forward
//! compatibility but reported as warnings, so a misspelt optional field shows
//! up in the Runtime screen instead of vanishing.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::platform;

/// File name of the manifest at a package root.
pub const MANIFEST_FILE: &str = "senclaw-runtime.json";

/// The only schema this crate reads. A newer one is refused by name rather
/// than half-understood.
pub const SCHEMA_VERSION: u32 = 1;

/// Placeholders any runtime's `entry` may use.
pub const SERVICE_PLACEHOLDERS: &[&str] = &[
    "host",
    "port",
    "token",
    "data_dir",
    "models_dir",
    "package_dir",
    "parent_pid",
];

/// Placeholders only a `mode: "model"` runtime may use — they describe the one
/// model the process is launched for.
pub const MODEL_PLACEHOLDERS: &[&str] = &["model_path", "model_id", "mmproj_path", "context_length"];

/// Top-level keys this schema defines; anything else is a warning.
const KNOWN_KEYS: &[&str] = &[
    "schemaVersion",
    "id",
    "name",
    "version",
    "description",
    "type",
    "slots",
    "formats",
    "capabilities",
    "platforms",
    "accelerator",
    "mode",
    "entry",
    "health",
    "idleTimeoutSecs",
    "api",
    "homepage",
    "releaseNotesUrl",
    "license",
];

/// What kind of engine a runtime is — the "type" filter of the Runtime screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeType {
    /// Loads language / embedding model files (llama.cpp, MLX).
    LlmEngine,
    /// Answers typed questions with probabilities (Laya, Jev online).
    Decision,
    Ocr,
    /// Speech to text.
    Asr,
    /// Text to speech.
    Tts,
}

/// What a user selects a runtime *for* — the "Runtime Selections" rows.
///
/// Model slots (`gguf`, `mlx`, `gturbo`) are chosen per model file format; the others per
/// capability, and the daemon routes that capability's legacy REST namespace
/// (see [`Slot::legacy_prefix`]) to whichever runtime fills the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Slot {
    Gguf,
    Mlx,
    /// A completed TurboFieldfare `.gturbo` directory (Gemma 4 26B-A4B).
    Gturbo,
    Decision,
    Ocr,
    Asr,
    Tts,
}

impl Slot {
    pub const ALL: [Slot; 7] = [
        Slot::Gguf,
        Slot::Mlx,
        Slot::Gturbo,
        Slot::Decision,
        Slot::Ocr,
        Slot::Asr,
        Slot::Tts,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Slot::Gguf => "gguf",
            Slot::Mlx => "mlx",
            Slot::Gturbo => "gturbo",
            Slot::Decision => "decision",
            Slot::Ocr => "ocr",
            Slot::Asr => "asr",
            Slot::Tts => "tts",
        }
    }

    pub fn parse(s: &str) -> Option<Slot> {
        Slot::ALL.into_iter().find(|slot| slot.as_str() == s)
    }

    /// Human label for the Runtime screen.
    pub fn label(self) -> &'static str {
        match self {
            Slot::Gguf => "GGUF",
            Slot::Mlx => "MLX",
            Slot::Gturbo => "TurboFieldfare",
            Slot::Decision => "Decision (System One)",
            Slot::Ocr => "OCR",
            Slot::Asr => "Speech to text",
            Slot::Tts => "Text to speech",
        }
    }

    /// The model format a model slot loads; `None` for capability slots.
    pub fn format(self) -> Option<ModelFormat> {
        match self {
            Slot::Gguf => Some(ModelFormat::Gguf),
            Slot::Mlx => Some(ModelFormat::Mlx),
            Slot::Gturbo => Some(ModelFormat::Gturbo),
            _ => None,
        }
    }

    /// The slot that loads files of `format`.
    pub fn for_format(format: ModelFormat) -> Slot {
        match format {
            ModelFormat::Gguf => Slot::Gguf,
            ModelFormat::Mlx => Slot::Mlx,
            ModelFormat::Gturbo => Slot::Gturbo,
        }
    }

    /// The daemon REST namespace a capability slot's runtime serves verbatim.
    /// These are the paths the old in-daemon engines answered, kept so every
    /// existing client works unchanged through the daemon's proxy.
    pub fn legacy_prefix(self) -> Option<&'static str> {
        match self {
            Slot::Decision => Some("/api/decision"),
            Slot::Ocr => Some("/api/ocr"),
            Slot::Asr => Some("/api/whisper"),
            Slot::Tts => Some("/api/tts"),
            Slot::Gguf | Slot::Mlx | Slot::Gturbo => None,
        }
    }

    /// The runtime type that may fill this slot.
    pub fn runtime_type(self) -> RuntimeType {
        match self {
            Slot::Gguf | Slot::Mlx | Slot::Gturbo => RuntimeType::LlmEngine,
            Slot::Decision => RuntimeType::Decision,
            Slot::Ocr => RuntimeType::Ocr,
            Slot::Asr => RuntimeType::Asr,
            Slot::Tts => RuntimeType::Tts,
        }
    }
}

impl std::fmt::Display for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Model file formats a `llm-engine` runtime loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelFormat {
    /// A single `.gguf` file (plus an optional `mmproj` projector for vision).
    Gguf,
    /// A directory of MLX safetensors with `config.json`.
    Mlx,
    /// A completed TurboFieldfare directory (`manifest.json` magic `GTURBO`).
    Gturbo,
}

impl ModelFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelFormat::Gguf => "gguf",
            ModelFormat::Mlx => "mlx",
            ModelFormat::Gturbo => "gturbo",
        }
    }
}

/// What a runtime (or a loaded model) can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    Chat,
    Embedding,
    Vision,
    Decision,
    Ocr,
    Asr,
    Tts,
}

/// How the daemon runs the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    /// One process per runtime; it manages its own models over its API.
    Service,
    /// One process per loaded model, passed on the command line
    /// (`{model_path}`) — how LM Studio and llama-server run model files.
    Model,
}

/// How to start the program.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Executable, relative to the package directory. Never absolute and never
    /// `..`: a package may only run what it ships.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment, rendered like `args`. The daemon's own launch
    /// variables (see [`crate::env`]) are always set and win on conflict.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Model mode only: arguments appended when the model being launched has
    /// the capability — e.g. `{"embedding": ["--embedding"],
    /// "vision": ["--mmproj", "{mmproj_path}"]}` for llama-server.
    #[serde(default)]
    pub capability_args: BTreeMap<Capability, Vec<String>>,
}

/// Readiness probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    #[serde(default = "default_health_path")]
    pub path: String,
    /// How long a start may take before the daemon gives up and kills it. A
    /// model-mode process answers 503 while its weights load, so this bounds
    /// the load too.
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout_secs: u64,
}

fn default_health_path() -> String {
    "/health".to_string()
}

fn default_startup_timeout() -> u64 {
    60
}

impl Default for Health {
    fn default() -> Self {
        Health { path: default_health_path(), startup_timeout_secs: default_startup_timeout() }
    }
}

/// Where the runtime's APIs live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiDecl {
    /// Base path of the OpenAI-compatible API (`/v1`). The daemon registers a
    /// loaded model as an OpenAI provider at `<proxy><openaiBase>`.
    #[serde(default = "default_openai_base")]
    pub openai_base: String,
}

fn default_openai_base() -> String {
    "/v1".to_string()
}

impl Default for ApiDecl {
    fn default() -> Self {
        ApiDecl { openai_base: default_openai_base() }
    }
}

/// A parsed `senclaw-runtime.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeManifest {
    pub schema_version: u32,
    /// Stable id, also the directory name under `runtimes/` (`sen-ocr`,
    /// `llama.cpp-metal`).
    pub id: String,
    pub name: String,
    /// Version string, also the package directory name. Upstream llama.cpp
    /// builds keep their tag (`b11201`).
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "type")]
    pub runtime_type: RuntimeType,
    pub slots: Vec<Slot>,
    #[serde(default)]
    pub formats: Vec<ModelFormat>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    pub platforms: Vec<String>,
    /// `metal`, `cpu`, `cuda`, `vulkan`, `rocm`… — shown, not interpreted.
    #[serde(default)]
    pub accelerator: Option<String>,
    pub mode: RunMode,
    pub entry: Entry,
    #[serde(default)]
    pub health: Health,
    /// Stop the process after this long without a request. `None` = the
    /// daemon's default for the mode; `Some(0)` = never.
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
    #[serde(default)]
    pub api: ApiDecl,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub release_notes_url: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
}

/// A manifest plus the non-fatal findings from reading it.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub manifest: RuntimeManifest,
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ManifestError {
    #[error("manifest is not valid JSON for schema {SCHEMA_VERSION}: {0}")]
    Json(String),
    #[error("unsupported schemaVersion {0} (this build reads {SCHEMA_VERSION})")]
    UnsupportedSchema(u32),
    #[error("invalid manifest: {0}")]
    Invalid(String),
    #[error("unknown placeholder `{{{0}}}`")]
    UnknownPlaceholder(String),
    #[error("placeholder `{{{0}}}` has no value in this launch")]
    MissingValue(String),
}

impl RuntimeManifest {
    /// Parse and validate manifest text.
    pub fn parse(text: &str) -> Result<Parsed, ManifestError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| ManifestError::Json(e.to_string()))?;
        let mut warnings = Vec::new();
        if let Some(obj) = value.as_object() {
            if let Some(v) = obj.get("schemaVersion").and_then(|v| v.as_u64()) {
                if v != u64::from(SCHEMA_VERSION) {
                    return Err(ManifestError::UnsupportedSchema(v as u32));
                }
            }
            for key in obj.keys() {
                if !KNOWN_KEYS.contains(&key.as_str()) {
                    warnings.push(format!("unknown field `{key}` ignored"));
                }
            }
        }
        let manifest: RuntimeManifest =
            serde_json::from_value(value).map_err(|e| ManifestError::Json(e.to_string()))?;
        manifest.validate()?;
        Ok(Parsed { manifest, warnings })
    }

    /// Read `senclaw-runtime.json` from a package directory.
    pub fn read_from_dir(dir: &Path) -> Result<Parsed, ManifestError> {
        let path = dir.join(MANIFEST_FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| ManifestError::Invalid(format!("cannot read {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    /// Every rule a well-formed manifest satisfies.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let bad = |msg: String| Err(ManifestError::Invalid(msg));
        if self.schema_version != SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema(self.schema_version));
        }
        if !valid_id(&self.id) {
            return bad(format!(
                "id `{}` must be 1–64 chars of a-z 0-9 . _ - starting with a letter or digit",
                self.id
            ));
        }
        if self.name.trim().is_empty() {
            return bad("name is empty".into());
        }
        if !valid_version(&self.version) {
            return bad(format!("version `{}` is not usable as a directory name", self.version));
        }
        if self.slots.is_empty() {
            return bad("slots is empty — the runtime could never be selected".into());
        }
        for slot in &self.slots {
            if slot.runtime_type() != self.runtime_type {
                return bad(format!(
                    "slot `{slot}` cannot be filled by a runtime of type `{}`",
                    type_name(self.runtime_type)
                ));
            }
        }
        let is_engine = self.runtime_type == RuntimeType::LlmEngine;
        match (is_engine, self.mode) {
            (true, RunMode::Service) => return bad("an llm-engine runtime must use mode `model`".into()),
            (false, RunMode::Model) => return bad("only an llm-engine runtime may use mode `model`".into()),
            _ => {}
        }
        if is_engine {
            if self.formats.is_empty() {
                return bad("an llm-engine runtime must declare the model formats it loads".into());
            }
            for slot in &self.slots {
                if let Some(format) = slot.format() {
                    if !self.formats.contains(&format) {
                        return bad(format!("slot `{slot}` needs format `{}` in formats", format.as_str()));
                    }
                }
            }
        } else if !self.formats.is_empty() {
            return bad("formats only apply to llm-engine runtimes".into());
        }
        if self.platforms.is_empty() {
            return bad("platforms is empty".into());
        }
        for p in &self.platforms {
            if !platform::is_known(p) {
                return bad(format!("unknown platform `{p}` (known: {})", platform::KNOWN.join(", ")));
            }
        }
        check_command(&self.entry.command)?;
        let model_mode = self.mode == RunMode::Model;
        for arg in &self.entry.args {
            check_placeholders(arg, model_mode)?;
        }
        for (key, value) in &self.entry.env {
            if key.is_empty() || key.contains('=') {
                return bad(format!("env key `{key}` is not a variable name"));
            }
            check_placeholders(value, model_mode)?;
        }
        if !self.entry.capability_args.is_empty() && !model_mode {
            return bad("capabilityArgs only apply to mode `model`".into());
        }
        for (cap, args) in &self.entry.capability_args {
            if !self.capabilities.contains(cap) {
                return bad(format!("capabilityArgs names `{cap:?}`, which is not in capabilities"));
            }
            for arg in args {
                check_placeholders(arg, true)?;
            }
        }
        if !self.health.path.starts_with('/') {
            return bad(format!("health.path `{}` must start with /", self.health.path));
        }
        if self.health.startup_timeout_secs == 0 || self.health.startup_timeout_secs > 3600 {
            return bad("health.startupTimeoutSecs must be 1–3600".into());
        }
        if !self.api.openai_base.starts_with('/') {
            return bad(format!("api.openaiBase `{}` must start with /", self.api.openai_base));
        }
        Ok(())
    }

    /// Whether the package runs on `platform_key`.
    pub fn supports_platform(&self, platform_key: &str) -> bool {
        self.platforms.iter().any(|p| p == platform_key)
    }

    /// The executable inside `package_dir`.
    pub fn command_path(&self, package_dir: &Path) -> Result<PathBuf, ManifestError> {
        check_command(&self.entry.command)?;
        Ok(package_dir.join(&self.entry.command))
    }

    /// Rendered argument list for one launch. `model_capabilities` selects
    /// which `capabilityArgs` are appended (model mode; ignored otherwise), in
    /// the manifest's key order.
    pub fn launch_args(
        &self,
        vars: &BTreeMap<&str, String>,
        model_capabilities: &[Capability],
    ) -> Result<Vec<String>, ManifestError> {
        let mut out = Vec::with_capacity(self.entry.args.len());
        for arg in &self.entry.args {
            out.push(render(arg, vars)?);
        }
        if self.mode == RunMode::Model {
            for (cap, args) in &self.entry.capability_args {
                if model_capabilities.contains(cap) {
                    for arg in args {
                        out.push(render(arg, vars)?);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Rendered `entry.env`.
    pub fn launch_env(&self, vars: &BTreeMap<&str, String>) -> Result<BTreeMap<String, String>, ManifestError> {
        self.entry
            .env
            .iter()
            .map(|(k, v)| Ok((k.clone(), render(v, vars)?)))
            .collect()
    }
}

fn type_name(t: RuntimeType) -> &'static str {
    match t {
        RuntimeType::LlmEngine => "llm-engine",
        RuntimeType::Decision => "decision",
        RuntimeType::Ocr => "ocr",
        RuntimeType::Asr => "asr",
        RuntimeType::Tts => "tts",
    }
}

/// Runtime ids: they become directory names and URL segments.
pub fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
        && !id.contains("..")
}

/// Versions become a directory name under the runtime's folder.
pub fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version != "."
        && !version.contains("..")
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

fn check_command(command: &str) -> Result<(), ManifestError> {
    if command.trim().is_empty() {
        return Err(ManifestError::Invalid("entry.command is empty".into()));
    }
    let path = Path::new(command);
    let escapes = path.is_absolute()
        || command.starts_with('/')
        || command.starts_with('\\')
        || path.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir));
    if escapes {
        return Err(ManifestError::Invalid(format!(
            "entry.command `{command}` must be a path inside the package (relative, no ..)"
        )));
    }
    Ok(())
}

/// A `{name}` token: letters, digits and `_`, starting with a letter or `_`.
/// Anything else between braces (JSON in an argument, `{{`) is literal text.
fn placeholder_at(s: &str, start: usize) -> Option<(&str, usize)> {
    let rest = &s[start + 1..];
    let end = rest.find('}')?;
    let name = &rest[..end];
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((name, start + 1 + end + 1))
}

fn check_placeholders(s: &str, model_mode: bool) -> Result<(), ManifestError> {
    let mut i = 0;
    while let Some(off) = s[i..].find('{') {
        let at = i + off;
        if s[at..].starts_with("{{") {
            i = at + 2;
            continue;
        }
        match placeholder_at(s, at) {
            Some((name, next)) => {
                let known = SERVICE_PLACEHOLDERS.contains(&name) || MODEL_PLACEHOLDERS.contains(&name);
                if !known {
                    return Err(ManifestError::UnknownPlaceholder(name.to_string()));
                }
                if !model_mode && MODEL_PLACEHOLDERS.contains(&name) {
                    return Err(ManifestError::Invalid(format!(
                        "placeholder `{{{name}}}` is only available to mode `model`"
                    )));
                }
                i = next;
            }
            None => i = at + 1,
        }
    }
    Ok(())
}

/// Substitute `{name}` tokens. `{{` and `}}` are literal braces; a known name
/// without a value and an unknown name are both errors.
pub fn render(template: &str, vars: &BTreeMap<&str, String>) -> Result<String, ManifestError> {
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    let bytes = template.as_bytes();
    while i < template.len() {
        if template[i..].starts_with("{{") {
            out.push('{');
            i += 2;
            continue;
        }
        if template[i..].starts_with("}}") {
            out.push('}');
            i += 2;
            continue;
        }
        if bytes[i] == b'{' {
            if let Some((name, next)) = placeholder_at(template, i) {
                let known = SERVICE_PLACEHOLDERS.contains(&name) || MODEL_PLACEHOLDERS.contains(&name);
                if !known {
                    return Err(ManifestError::UnknownPlaceholder(name.to_string()));
                }
                let value = vars.get(name).ok_or_else(|| ManifestError::MissingValue(name.to_string()))?;
                out.push_str(value);
                i = next;
                continue;
            }
        }
        let ch = template[i..].chars().next().expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service_manifest() -> &'static str {
        r#"{
          "schemaVersion": 1,
          "id": "sen-ocr",
          "name": "SenClaw OCR",
          "version": "0.1.0",
          "type": "ocr",
          "slots": ["ocr"],
          "capabilities": ["ocr"],
          "platforms": ["darwin-arm64", "linux-x64"],
          "accelerator": "cpu",
          "mode": "service",
          "entry": {
            "command": "bin/sen-ocr",
            "args": ["serve", "--host", "{host}", "--port", "{port}"],
            "env": {"SEN_OCR_CACHE": "{data_dir}/cache"}
          },
          "idleTimeoutSecs": 300
        }"#
    }

    fn model_manifest() -> &'static str {
        r#"{
          "schemaVersion": 1,
          "id": "llama.cpp-metal",
          "name": "Metal llama.cpp",
          "version": "b11201",
          "type": "llm-engine",
          "slots": ["gguf"],
          "formats": ["gguf"],
          "capabilities": ["chat", "embedding", "vision"],
          "platforms": ["darwin-arm64"],
          "mode": "model",
          "entry": {
            "command": "llama-b11201/llama-server",
            "args": ["-m", "{model_path}", "--host", "{host}", "--port", "{port}",
                     "--api-key", "{token}", "-c", "{context_length}"],
            "capabilityArgs": {"embedding": ["--embedding"], "vision": ["--mmproj", "{mmproj_path}"]}
          },
          "health": {"path": "/health", "startupTimeoutSecs": 600}
        }"#
    }

    fn vars() -> BTreeMap<&'static str, String> {
        let mut v = BTreeMap::new();
        v.insert("host", "127.0.0.1".to_string());
        v.insert("port", "41234".to_string());
        v.insert("token", "t0k".to_string());
        v.insert("data_dir", "/d".to_string());
        v.insert("model_path", "/m/q.gguf".to_string());
        v.insert("mmproj_path", "/m/mmproj.gguf".to_string());
        v.insert("context_length", "8192".to_string());
        v
    }

    #[test]
    fn parses_a_service_manifest() {
        let parsed = RuntimeManifest::parse(service_manifest()).unwrap();
        assert!(parsed.warnings.is_empty());
        let m = parsed.manifest;
        assert_eq!(m.slots, vec![Slot::Ocr]);
        assert_eq!(m.health, Health::default());
        assert_eq!(m.idle_timeout_secs, Some(300));
        let args = m.launch_args(&vars(), &[]).unwrap();
        assert_eq!(args, vec!["serve", "--host", "127.0.0.1", "--port", "41234"]);
        assert_eq!(m.launch_env(&vars()).unwrap()["SEN_OCR_CACHE"], "/d/cache");
    }

    #[test]
    fn model_manifest_appends_capability_args_for_what_the_model_has() {
        let m = RuntimeManifest::parse(model_manifest()).unwrap().manifest;
        let chat = m.launch_args(&vars(), &[Capability::Chat]).unwrap();
        assert!(!chat.contains(&"--embedding".to_string()));
        assert!(!chat.contains(&"--mmproj".to_string()));
        let vision = m.launch_args(&vars(), &[Capability::Chat, Capability::Vision]).unwrap();
        assert_eq!(&vision[vision.len() - 2..], &["--mmproj", "/m/mmproj.gguf"]);
        let embed = m.launch_args(&vars(), &[Capability::Embedding]).unwrap();
        assert_eq!(embed.last().unwrap(), "--embedding");
    }

    #[test]
    fn misspelt_enum_values_fail_loudly() {
        let text = service_manifest().replace(r#"["ocr"],
          "capabilities""#, r#"["orc"],
          "capabilities""#);
        assert!(matches!(RuntimeManifest::parse(&text), Err(ManifestError::Json(_))));
        let text = service_manifest().replace(r#""mode": "service""#, r#""mode": "services""#);
        assert!(matches!(RuntimeManifest::parse(&text), Err(ManifestError::Json(_))));
    }

    #[test]
    fn unknown_placeholders_are_refused_at_parse_time() {
        let text = service_manifest().replace("{port}", "{prot}");
        assert_eq!(
            RuntimeManifest::parse(&text).unwrap_err(),
            ManifestError::UnknownPlaceholder("prot".into())
        );
    }

    #[test]
    fn model_placeholders_are_refused_in_service_mode() {
        let text = service_manifest().replace("{port}", "{model_path}");
        assert!(matches!(RuntimeManifest::parse(&text), Err(ManifestError::Invalid(_))));
    }

    #[test]
    fn unknown_keys_are_warnings_not_errors() {
        let text = service_manifest().replace(r#""idleTimeoutSecs": 300"#, r#""idleTimeoutSec": 300"#);
        let parsed = RuntimeManifest::parse(&text).unwrap();
        assert_eq!(parsed.manifest.idle_timeout_secs, None);
        assert_eq!(parsed.warnings, vec!["unknown field `idleTimeoutSec` ignored".to_string()]);
    }

    #[test]
    fn newer_schema_is_refused_by_name() {
        let text = service_manifest().replace(r#""schemaVersion": 1"#, r#""schemaVersion": 2"#);
        assert_eq!(RuntimeManifest::parse(&text).unwrap_err(), ManifestError::UnsupportedSchema(2));
    }

    #[test]
    fn command_must_stay_inside_the_package() {
        for cmd in ["/usr/bin/sh", "../x/bin", "bin/../../x", "\\\\server\\x"] {
            let text = service_manifest().replace("bin/sen-ocr", cmd);
            assert!(RuntimeManifest::parse(&text).is_err(), "{cmd} accepted");
        }
        let m = RuntimeManifest::parse(service_manifest()).unwrap().manifest;
        assert_eq!(m.command_path(Path::new("/pkg")).unwrap(), PathBuf::from("/pkg/bin/sen-ocr"));
    }

    #[test]
    fn slot_and_type_must_agree() {
        let text = service_manifest().replace(r#""slots": ["ocr"]"#, r#""slots": ["tts"]"#);
        assert!(matches!(RuntimeManifest::parse(&text), Err(ManifestError::Invalid(_))));
        let text = model_manifest().replace(r#""formats": ["gguf"]"#, r#""formats": ["mlx"]"#);
        assert!(matches!(RuntimeManifest::parse(&text), Err(ManifestError::Invalid(_))));
    }

    #[test]
    fn render_keeps_json_braces_and_escapes() {
        let v = vars();
        assert_eq!(render(r#"{"port": {port}}"#, &v).unwrap(), r#"{"port": 41234}"#);
        assert_eq!(render("{{port}}", &v).unwrap(), "{port}");
        assert_eq!(render("{model_id}", &v).unwrap_err(), ManifestError::MissingValue("model_id".into()));
        assert_eq!(render("héllo {host}", &v).unwrap(), "héllo 127.0.0.1");
    }

    #[test]
    fn ids_and_versions() {
        assert!(valid_id("llama.cpp-metal"));
        assert!(valid_id("sen-mlx"));
        assert!(!valid_id("Sen-MLX"));
        assert!(!valid_id("-x"));
        assert!(!valid_id("a..b"));
        assert!(valid_version("b11201"));
        assert!(valid_version("0.1.0-beta.1+build.5"));
        assert!(!valid_version("../0.1"));
        assert!(!valid_version("0.1/x"));
    }

    #[test]
    fn slot_helpers_round_trip() {
        for slot in Slot::ALL {
            assert_eq!(Slot::parse(slot.as_str()), Some(slot));
        }
        assert_eq!(Slot::Asr.legacy_prefix(), Some("/api/whisper"));
        assert_eq!(Slot::for_format(ModelFormat::Mlx), Slot::Mlx);
        assert_eq!(Slot::for_format(ModelFormat::Gturbo), Slot::Gturbo);
        assert_eq!(Slot::Gturbo.format(), Some(ModelFormat::Gturbo));
    }
}

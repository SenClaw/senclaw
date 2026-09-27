//! JSON bodies every runtime answers with, whatever else it serves.

use serde::{Deserialize, Serialize};

use crate::manifest::{Capability, RunMode};

/// `GET /health`. 200 once the runtime can take requests; a model-mode
/// process answers 503 with `status: "loading"` while its weights load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    pub id: String,
    pub version: String,
}

/// `GET /runtime/info` — what the process is and what it holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInfo {
    pub id: String,
    pub version: String,
    pub mode: RunMode,
    pub capabilities: Vec<Capability>,
    pub pid: u32,
    /// Runtime-specific detail (loaded models, memory, backend in use).
    #[serde(default)]
    pub detail: serde_json::Value,
}

/// Error body. `error` is the message a person reads — the same field the
/// daemon's own errors carry, so a client that shows `body.error` keeps
/// working when a route moves from the daemon into a runtime. `code` is for
/// programs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ErrorBody {
    pub fn new(message: impl Into<String>) -> ErrorBody {
        ErrorBody { error: message.into(), code: None }
    }

    pub fn with_code(message: impl Into<String>, code: &str) -> ErrorBody {
        ErrorBody { error: message.into(), code: Some(code.to_string()) }
    }
}

/// Machine-readable error codes shared by the daemon's runtime proxy and the
/// runtimes, so clients can react (e.g. link to Settings → Runtime).
pub mod codes {
    /// No runtime is installed for the slot the request needs.
    pub const RUNTIME_NOT_INSTALLED: &str = "runtime_not_installed";
    /// Installed but not selected for the slot.
    pub const RUNTIME_NOT_SELECTED: &str = "runtime_not_selected";
    /// The process would not start or never became healthy.
    pub const RUNTIME_START_FAILED: &str = "runtime_start_failed";
    /// The package does not run on this machine.
    pub const RUNTIME_INCOMPATIBLE: &str = "runtime_incompatible";
    /// Missing or wrong bearer token.
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// Still loading weights.
    pub const LOADING: &str = "loading";
}

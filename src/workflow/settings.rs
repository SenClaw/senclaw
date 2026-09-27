//! Workflow runtime settings — user-tunable knobs persisted to a small JSON
//! file next to the run store and applied live through shared atomics.
//!
//!   - `llm_parallel`: how many AGENT steps may talk to the LLM at once.
//!     Defaults to 1 because many providers (or single local models) reject
//!     concurrent requests — parallel sibling steps then fail or hang.
//!     Steps waiting for a slot stay `pending`, so their timeout only starts
//!     when they actually run.
//!   - `agent_retries`: extra attempts when an agent step ends with a session
//!     error or no text at all (a step's whole point is its `result`).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub const DEFAULT_LLM_PARALLEL: usize = 1;
pub const DEFAULT_AGENT_RETRIES: usize = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSettings {
    /// Concurrent agent (LLM) steps per daemon. Min 1.
    #[serde(default = "default_llm_parallel")]
    pub llm_parallel: usize,
    /// Extra attempts for an agent step that errored or produced no text.
    #[serde(default = "default_agent_retries")]
    pub agent_retries: usize,
}

fn default_llm_parallel() -> usize {
    DEFAULT_LLM_PARALLEL
}
fn default_agent_retries() -> usize {
    DEFAULT_AGENT_RETRIES
}

impl Default for WorkflowSettings {
    fn default() -> Self {
        Self {
            llm_parallel: DEFAULT_LLM_PARALLEL,
            agent_retries: DEFAULT_AGENT_RETRIES,
        }
    }
}

impl WorkflowSettings {
    pub fn clamped(mut self) -> Self {
        self.llm_parallel = self.llm_parallel.clamp(1, 16);
        self.agent_retries = self.agent_retries.min(5);
        self
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Self>(&raw).ok())
            .unwrap_or_default()
            .clamped()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default())?;
        std::fs::rename(&tmp, path)
    }
}

/// Live handles the executor consults on every dispatch/attempt — updating
/// them through the settings API takes effect immediately, mid-run included.
#[derive(Clone)]
pub struct LiveWorkflowSettings {
    pub llm_parallel: Arc<AtomicUsize>,
    pub agent_retries: Arc<AtomicUsize>,
}

impl LiveWorkflowSettings {
    pub fn new(s: &WorkflowSettings) -> Self {
        Self {
            llm_parallel: Arc::new(AtomicUsize::new(s.llm_parallel.max(1))),
            agent_retries: Arc::new(AtomicUsize::new(s.agent_retries)),
        }
    }

    pub fn apply(&self, s: &WorkflowSettings) {
        self.llm_parallel
            .store(s.llm_parallel.max(1), Ordering::Relaxed);
        self.agent_retries.store(s.agent_retries, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> WorkflowSettings {
        WorkflowSettings {
            llm_parallel: self.llm_parallel.load(Ordering::Relaxed),
            agent_retries: self.agent_retries.load(Ordering::Relaxed),
        }
    }
}

impl Default for LiveWorkflowSettings {
    fn default() -> Self {
        Self::new(&WorkflowSettings::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_gives_defaults() {
        let s = WorkflowSettings::load(Path::new("/nonexistent/x.json"));
        assert_eq!(s.llm_parallel, 1);
        assert_eq!(s.agent_retries, 1);
    }

    #[test]
    fn save_load_roundtrip_and_clamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow-settings.json");
        WorkflowSettings {
            llm_parallel: 0,
            agent_retries: 99,
        }
        .clamped()
        .save(&path)
        .unwrap();
        let s = WorkflowSettings::load(&path);
        assert_eq!(s.llm_parallel, 1); // clamped up
        assert_eq!(s.agent_retries, 5); // clamped down
    }

    #[test]
    fn live_settings_apply_and_snapshot() {
        let live = LiveWorkflowSettings::default();
        live.apply(&WorkflowSettings {
            llm_parallel: 3,
            agent_retries: 2,
        });
        let snap = live.snapshot();
        assert_eq!(snap.llm_parallel, 3);
        assert_eq!(snap.agent_retries, 2);
    }
}

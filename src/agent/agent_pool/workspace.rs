//! Feishu credentials and workspace state file (de)serialization.

use serde::{Deserialize, Serialize};

/// Resolved Feishu credentials returned by [`AgentPool::resolve_feishu_credentials`].
#[derive(Debug, Clone)]
pub struct FeishuCredentials {
    pub app_id: String,
    pub app_secret: String,
    pub domain: Option<String>,
}

/// On-disk workspace state: `~/.senclaw/workspace-state-{folder}.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceStateFile {
    #[serde(rename = "currentDir")]
    pub current_dir: String,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: String,
}

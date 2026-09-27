//! Workspace MCP server. Port target: src-old/mcp/workspace-server.ts
//!
//! Tools: workspace_switch, workspace_reset, workspace_info.
//! Manages agent working directory via a state file watched by AgentPool.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::mcp::schedule_server::ToolResult;

use rmcp::ServiceExt;

// ===== MCP stdio server =====

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct WorkspaceSwitchParams {
    target_path: String,
}

#[derive(Clone)]
pub struct McpWorkspaceServer {
    state_file: PathBuf,
    default_workspace: PathBuf,
    allowed_work_dirs: Option<Vec<String>>,
}

impl McpWorkspaceServer {
    /// Build from the workspace env pair, or `None` when either is absent.
    /// See [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        let (Ok(state_file), Ok(default_workspace)) = (
            std::env::var("SENCLAW_WORKSPACE_STATE_FILE"),
            std::env::var("SENCLAW_DEFAULT_WORKSPACE"),
        ) else {
            return Ok(None);
        };
        // Empty is a real setting ("switching disabled"), not a missing var.
        let allowed_raw = std::env::var("SENCLAW_ALLOWED_WORK_DIRS").unwrap_or_default();
        let allowed_work_dirs: Option<Vec<String>> = if allowed_raw.is_empty() {
            None
        } else {
            Some(serde_json::from_str(&allowed_raw).context("parse SENCLAW_ALLOWED_WORK_DIRS")?)
        };
        Ok(Some(Self {
            state_file: PathBuf::from(state_file),
            default_workspace: PathBuf::from(default_workspace),
            allowed_work_dirs,
        }))
    }

    fn inner(&self) -> WorkspaceServer {
        WorkspaceServer::new(
            &self.state_file,
            &self.default_workspace,
            self.allowed_work_dirs.clone(),
        )
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpWorkspaceServer {
    #[rmcp::tool(description = "Switch the agent workspace directory")]
    fn workspace_switch(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            WorkspaceSwitchParams,
        >,
    ) -> String {
        self.inner().workspace_switch(&p.target_path).content
    }

    #[rmcp::tool(description = "Reset workspace to default directory")]
    fn workspace_reset(&self) -> String {
        self.inner().workspace_reset().content
    }

    #[rmcp::tool(description = "Show current workspace info")]
    fn workspace_info(&self) -> String {
        self.inner().workspace_info().content
    }
}

/// Start the workspace MCP server over stdio.
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpWorkspaceServer::from_env()?
        .context("SENCLAW_WORKSPACE_STATE_FILE / SENCLAW_DEFAULT_WORKSPACE not set")?;

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceState {
    #[serde(rename = "currentDir")]
    current_dir: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}

pub struct WorkspaceServer {
    state_file: PathBuf,
    default_workspace: PathBuf,
    /// `None` = switching disabled entirely. `Some(vec)` = allowed directories.
    allowed_work_dirs: Option<Vec<PathBuf>>,
}

impl WorkspaceServer {
    pub fn new(
        state_file: &Path,
        default_workspace: &Path,
        allowed_work_dirs: Option<Vec<String>>,
    ) -> Self {
        Self {
            state_file: state_file.to_path_buf(),
            default_workspace: default_workspace.to_path_buf(),
            allowed_work_dirs: allowed_work_dirs
                .map(|v| v.into_iter().map(PathBuf::from).collect()),
        }
    }

    fn read_state(&self) -> WorkspaceState {
        fs::read_to_string(&self.state_file)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_else(|| WorkspaceState {
                current_dir: self.default_workspace.to_string_lossy().to_string(),
                updated_at: chrono::Utc::now().to_rfc3339(),
            })
    }

    fn write_state(&self, new_dir: &str) {
        let state = WorkspaceState {
            current_dir: new_dir.to_owned(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Some(parent) = self.state_file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(
            &self.state_file,
            serde_json::to_string_pretty(&state).unwrap_or_default(),
        );
    }

    fn is_path_allowed(&self, target_path: &Path) -> bool {
        match &self.allowed_work_dirs {
            None => false,
            Some(dirs) => {
                let normalized = target_path
                    .canonicalize()
                    .unwrap_or_else(|_| target_path.to_path_buf());
                dirs.iter().any(|allowed| {
                    let ok_normalized = allowed.canonicalize().unwrap_or_else(|_| allowed.clone());
                    normalized == ok_normalized || normalized.starts_with(&ok_normalized)
                })
            }
        }
    }

    // ===== workspace_switch =====

    pub fn workspace_switch(&self, target_path: &str) -> ToolResult {
        if self.allowed_work_dirs.is_none() {
            return ToolResult::err(
                "This agent does not have workdir switching enabled (allowedWorkDirs not configured)"
                    .into(),
            );
        }

        let resolved = PathBuf::from(target_path);

        if !resolved.exists() {
            return ToolResult::err(format!("Directory does not exist: {}", resolved.display()));
        }

        if !resolved.is_dir() {
            return ToolResult::err(format!("Path is not a directory: {}", resolved.display()));
        }

        if !self.is_path_allowed(&resolved) {
            let list = match &self.allowed_work_dirs {
                Some(dirs) => dirs
                    .iter()
                    .map(|d| format!("  - {}", d.display()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => String::new(),
            };
            return ToolResult::err(format!(
                "Directory is not in allowedWorkDirs:\n{}\n\nAuthorized directories:\n{}",
                resolved.display(),
                if list.is_empty() {
                    "  (No pre-authorized directories yet. Configure allowedWorkDirs in ~/.senclaw/config.json)"
                        .into()
                } else {
                    list
                },
            ));
        }

        let resolved_str = resolved.to_string_lossy().to_string();
        self.write_state(&resolved_str);
        ToolResult::ok(format!("Workdir switched to: {resolved_str}"))
    }

    // ===== workspace_reset =====

    pub fn workspace_reset(&self) -> ToolResult {
        let default = self.default_workspace.to_string_lossy().to_string();
        self.write_state(&default);
        ToolResult::ok(format!("Workdir reset to default workspace: {default}"))
    }

    // ===== workspace_info =====

    pub fn workspace_info(&self) -> ToolResult {
        let state = self.read_state();
        let allowed_dirs = match &self.allowed_work_dirs {
            None => "(disabled)".to_owned(),
            Some(dirs) => dirs
                .iter()
                .map(|d| d.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        };
        let info = serde_json::json!({
            "currentDir": state.current_dir,
            "defaultWorkspace": self.default_workspace.to_string_lossy(),
            "allowedWorkDirs": allowed_dirs,
            "isAtDefault": state.current_dir == self.default_workspace.to_string_lossy(),
        });
        ToolResult::ok(serde_json::to_string_pretty(&info).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn workspace_switch_reset_info_flow() {
        let tmp = TempDir::new().unwrap();
        let state_file = tmp.path().join("workspace-state.json");
        let default = tmp.path().join("default");
        let allowed = tmp.path().join("allowed");
        fs::create_dir_all(&default).unwrap();
        fs::create_dir_all(&allowed).unwrap();

        let srv = WorkspaceServer::new(
            &state_file,
            &default,
            Some(vec![
                default.to_string_lossy().to_string(),
                allowed.to_string_lossy().to_string(),
            ]),
        );

        // info at default
        let info = srv.workspace_info();
        assert!(!info.is_error);

        // switch to allowed dir
        let sw = srv.workspace_switch(&allowed.to_string_lossy().to_string());
        assert!(!sw.is_error, "{}", sw.content);

        // verify state file updated
        let raw = fs::read_to_string(&state_file).unwrap();
        let state: WorkspaceState = serde_json::from_str(&raw).unwrap();
        assert_eq!(state.current_dir, allowed.to_string_lossy().to_string());

        // reset
        let reset = srv.workspace_reset();
        assert!(!reset.is_error);

        let state: WorkspaceState =
            serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
        assert_eq!(state.current_dir, default.to_string_lossy().to_string());
    }

    #[test]
    fn workspace_switch_rejects_unauthorized() {
        let tmp = TempDir::new().unwrap();
        let state_file = tmp.path().join("ws-state.json");
        let default = tmp.path().join("default");
        let other = tmp.path().join("other");
        fs::create_dir_all(&default).unwrap();
        fs::create_dir_all(&other).unwrap();

        let srv = WorkspaceServer::new(
            &state_file,
            &default,
            Some(vec![default.to_string_lossy().to_string()]),
        );
        let sw = srv.workspace_switch(&other.to_string_lossy().to_string());
        assert!(sw.is_error);
    }

    #[test]
    fn workspace_disabled_when_allowed_dirs_is_none() {
        let tmp = TempDir::new().unwrap();
        let srv = WorkspaceServer::new(
            &tmp.path().join("ws-state.json"),
            &tmp.path().join("default"),
            None,
        );
        let sw = srv.workspace_switch("/tmp");
        assert!(sw.is_error);
    }
}

//! Virtual MCP server. Port target: src-old/mcp/virtual-server.ts
//!
//! Tools: list_personas, run_persona.
//! Provides blocking virtual-agent task execution for admin agent tool_use context.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::mcp::schedule_server::ToolResult;

use rmcp::ServiceExt;

// ===== Persona definition =====

#[derive(Debug, Clone)]
pub struct PersonaConfig {
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub file_path: PathBuf,
}

/// Trait for running a persona with a prompt. Implementations use sema-core.
#[async_trait::async_trait]
pub trait VirtualWorkerPool: Send + Sync {
    async fn run(
        &self,
        persona: &PersonaConfig,
        prompt: &str,
        workspace: &Path,
        timeout_secs: Option<u64>,
    ) -> anyhow::Result<VirtualRunResult>;
}

#[derive(Debug, Clone)]
pub struct VirtualRunResult {
    pub result: String,
    pub duration_ms: u64,
}

pub struct VirtualServer {
    personas: Vec<PersonaConfig>,
    pool: Option<Box<dyn VirtualWorkerPool>>,
    admin_folder: String,
    default_workspace: PathBuf,
}

impl VirtualServer {
    pub fn new(
        agents_config_dir: &Path,
        admin_folder: &str,
        default_workspace: &Path,
        pool: Option<Box<dyn VirtualWorkerPool>>,
    ) -> Self {
        let personas = Self::scan_personas(agents_config_dir);
        Self {
            personas,
            pool,
            admin_folder: admin_folder.to_owned(),
            default_workspace: default_workspace.to_path_buf(),
        }
    }

    fn scan_personas(dir: &Path) -> Vec<PersonaConfig> {
        let mut personas = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "md") {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_owned();
                    let content = fs::read_to_string(&path).unwrap_or_default();
                    let description = content
                        .lines()
                        .find(|l| !l.starts_with('#') && !l.is_empty())
                        .map(|l| l.trim().trim_matches(&['"', '\''][..]).to_owned())
                        .unwrap_or_else(|| "(no description)".into());
                    personas.push(PersonaConfig {
                        name,
                        description,
                        tools: None,
                        file_path: path,
                    });
                }
            }
        }
        personas
    }

    fn read_current_workspace(&self) -> PathBuf {
        let state_file = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".senclaw")
            .join(format!("workspace-state-{}.json", self.admin_folder));
        fs::read_to_string(&state_file)
            .ok()
            .and_then(|raw| {
                let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
                v.get("currentDir")
                    .and_then(|d| d.as_str())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| self.default_workspace.clone())
    }

    // ===== list_personas =====

    pub fn list_personas(&self) -> ToolResult {
        if self.personas.is_empty() {
            return ToolResult::ok(
                "No personas configured. Add .md files to ~/senclaw/virtual-agents/".into(),
            );
        }
        let lines: Vec<String> = self
            .personas
            .iter()
            .map(|p| format!("- **{}**: {}", p.name, p.description))
            .collect();
        ToolResult::ok(lines.join("\n"))
    }

    // ===== run_persona =====

    pub async fn run_persona(
        &self,
        persona_name: &str,
        prompt: &str,
        timeout_seconds: Option<u64>,
    ) -> ToolResult {
        let persona = match self.personas.iter().find(|p| p.name == persona_name) {
            Some(p) => p.clone(),
            None => {
                return ToolResult::err(format!(
                    "Persona \"{persona_name}\" not found. Use list_personas to see available personas."
                ));
            }
        };

        let pool = match &self.pool {
            Some(p) => p,
            None => {
                return ToolResult::err(
                    "VirtualWorkerPool not initialized. Cannot run persona.".into(),
                );
            }
        };

        let timeout = timeout_seconds.map(|t| t.clamp(10, 1800)).unwrap_or(600);

        let workspace = self.read_current_workspace();

        match pool.run(&persona, prompt, &workspace, Some(timeout)).await {
            Ok(run_result) => {
                let json = serde_json::json!({
                    "persona": persona_name,
                    "result": if run_result.result.is_empty() {
                        "(completed with no text output)"
                    } else {
                        &run_result.result
                    },
                    "duration_ms": run_result.duration_ms,
                });
                ToolResult::ok(serde_json::to_string_pretty(&json).unwrap_or_default())
            }
            Err(e) => ToolResult::err(format!("run_persona failed: {e}")),
        }
    }
}

// ===== MCP stdio server =====

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct RunPersonaParams {
    #[serde(rename = "personaName")]
    persona_name: String,
    prompt: String,
    #[serde(default)]
    #[serde(rename = "timeoutSeconds")]
    timeout_seconds: Option<u64>,
}

#[derive(Clone)]
pub struct McpVirtualServer {
    agents_config_dir: PathBuf,
    admin_folder: String,
    default_workspace: PathBuf,
}

impl McpVirtualServer {
    /// Build from the virtual-agent env trio, or `None` when any is absent. See
    /// [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        let (Ok(agents_config_dir), Ok(admin_folder), Ok(default_workspace)) = (
            std::env::var("SENCLAW_AGENTS_CONFIG_DIR"),
            std::env::var("SENCLAW_ADMIN_FOLDER"),
            std::env::var("SENCLAW_DEFAULT_WORKSPACE"),
        ) else {
            return Ok(None);
        };
        Ok(Some(Self {
            agents_config_dir: PathBuf::from(agents_config_dir),
            admin_folder,
            default_workspace: PathBuf::from(default_workspace),
        }))
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpVirtualServer {
    #[rmcp::tool(description = "List available virtual personas")]
    fn list_personas(&self) -> String {
        let srv = VirtualServer::new(
            &self.agents_config_dir,
            &self.admin_folder,
            &self.default_workspace,
            None,
        );
        srv.list_personas().content
    }

    #[rmcp::tool(description = "Run a virtual persona with a prompt")]
    async fn run_persona(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            RunPersonaParams,
        >,
    ) -> String {
        let srv = VirtualServer::new(
            &self.agents_config_dir,
            &self.admin_folder,
            &self.default_workspace,
            None,
        );
        srv.run_persona(&p.persona_name, &p.prompt, p.timeout_seconds)
            .await
            .content
    }
}

/// Start the virtual MCP server over stdio.
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpVirtualServer::from_env()?.context(
        "SENCLAW_AGENTS_CONFIG_DIR / SENCLAW_ADMIN_FOLDER / SENCLAW_DEFAULT_WORKSPACE not set",
    )?;

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_personas_from_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::write(
            tmp.path().join("coder.md"),
            "# Coder\nWrites production code",
        )
        .unwrap();
        fs::write(tmp.path().join("tester.md"), "# Tester\nRuns test suites").unwrap();
        fs::write(tmp.path().join("readme.txt"), "not a persona").unwrap();

        let personas = VirtualServer::scan_personas(tmp.path());
        assert_eq!(personas.len(), 2);
        let names: Vec<&str> = personas.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"coder"));
        assert!(names.contains(&"tester"));
    }

    #[test]
    fn list_personas_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let empty_dir = tmp.path().join("empty");
        fs::create_dir_all(&empty_dir).unwrap();
        let srv = VirtualServer::new(&empty_dir, "main", Path::new("/tmp"), None);
        let result = srv.list_personas();
        assert!(!result.is_error);
        assert!(result.content.contains("No personas configured"));
    }
}

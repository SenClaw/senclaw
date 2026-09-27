//! Project-documentation MCP server (`senclaw-docs`).
//!
//! Tools: `doc_write`, `doc_read`, `doc_list`, `doc_index_rebuild`.
//!
//! These write into **the project the chat is working in**, never into the
//! user's global wiki. The distinction is the point: `wiki_*` is one personal
//! knowledge base shared across every project, while a document about a
//! repository's code has to travel with that repository.
//!
//! The working directory is read from the workspace state file on **every
//! call**, not captured once at spawn. A chat can switch workspace mid-session
//! (`workspace_switch`), and a snapshot taken at spawn would keep writing into
//! the directory the session started in — silently, since the write succeeds.

use std::path::PathBuf;

use anyhow::Result;
use rmcp::ServiceExt;

use crate::docs::DocsStore;

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct DocWriteParams {
    /// Path relative to the project's `docs/` directory, ending in `.md`.
    path: String,
    /// The full markdown body, including its `#` heading.
    content: String,
    /// Replace the file when it already exists. Off by default so a
    /// hand-written document is never lost to a guess.
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct DocReadParams {
    /// Path relative to the project's `docs/` directory.
    path: String,
}

#[derive(Clone)]
pub struct McpDocsServer {
    state_file: PathBuf,
    default_workspace: PathBuf,
}

impl McpDocsServer {
    /// Built from the same workspace env pair the workspace server uses, or
    /// `None` when the chat has no workspace at all (a plain chat rather than
    /// a code session). See
    /// [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        let (Ok(state_file), Ok(default_workspace)) = (
            std::env::var("SENCLAW_WORKSPACE_STATE_FILE"),
            std::env::var("SENCLAW_DEFAULT_WORKSPACE"),
        ) else {
            return Ok(None);
        };
        Ok(Some(Self {
            state_file: PathBuf::from(state_file),
            default_workspace: PathBuf::from(default_workspace),
        }))
    }

    /// The chat's working directory right now.
    fn working_dir(&self) -> PathBuf {
        std::fs::read_to_string(&self.state_file)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| {
                v.get("currentDir")
                    .and_then(|d| d.as_str())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| self.default_workspace.clone())
    }

    fn store(&self) -> DocsStore {
        DocsStore::for_working_dir(self.working_dir())
    }
}

fn err(message: String) -> String {
    serde_json::json!({ "ok": false, "error": message }).to_string()
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpDocsServer {
    #[rmcp::tool(
        description = "Write a markdown document into THIS PROJECT's docs/ directory and refresh \
                       docs/README.md. Use this when the user asks for documentation, or after a \
                       change to behaviour, a contract, or a decision. Returns the absolute path \
                       to quote back to the user. Not for personal notes across projects — that \
                       is wiki_write."
    )]
    fn doc_write(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            DocWriteParams,
        >,
    ) -> String {
        let store = self.store();
        // Creating a docs/ tree inside somebody's project is a decision, so it
        // is only automatic once the project has made it. Otherwise say so and
        // let the agent ask.
        let adopting = !store.exists();
        match store.write(&p.path, &p.content, p.overwrite) {
            Ok(out) => {
                let index = store.rebuild_index().ok().flatten();
                serde_json::json!({
                    "ok": true,
                    "path": out.absolute_path.to_string_lossy(),
                    "relativePath": out.relative_path,
                    "created": out.created,
                    "createdDocsDirectory": adopting,
                    "index": index.map(|p| p.to_string_lossy().to_string()),
                    "note": "Tell the user this path. The document is written but NOT committed.",
                })
                .to_string()
            }
            Err(e) => err(format!("{e:#}")),
        }
    }

    #[rmcp::tool(description = "Read one markdown document from this project's docs/ directory")]
    fn doc_read(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            DocReadParams,
        >,
    ) -> String {
        match self.store().read(&p.path) {
            Ok(text) => serde_json::json!({ "ok": true, "path": p.path, "content": text })
                .to_string(),
            Err(e) => err(format!("{e:#}")),
        }
    }

    #[rmcp::tool(
        description = "List this project's documentation: every .md under docs/, with its title \
                       and one-line summary, plus which language the existing docs are written in"
    )]
    fn doc_list(&self) -> String {
        let store = self.store();
        let entries: Vec<serde_json::Value> = store
            .list()
            .into_iter()
            .map(|e| {
                serde_json::json!({ "path": e.path, "title": e.title, "summary": e.summary })
            })
            .collect();
        serde_json::json!({
            "ok": true,
            "root": store.root().to_string_lossy(),
            "exists": store.exists(),
            // So a new document matches the language the project already uses
            // rather than the language of the question that triggered it.
            "language": store.language().as_str(),
            "documents": entries,
        })
        .to_string()
    }

    #[rmcp::tool(
        description = "Regenerate the table in docs/README.md from the files on disk. Runs \
                       automatically after doc_write; call it by hand after deleting or renaming \
                       a document."
    )]
    fn doc_index_rebuild(&self) -> String {
        let store = self.store();
        match store.rebuild_index() {
            Ok(Some(path)) => {
                serde_json::json!({ "ok": true, "index": path.to_string_lossy() }).to_string()
            }
            Ok(None) => err(format!(
                "this project has no docs/ directory yet ({})",
                store.root().display()
            )),
            Err(e) => err(format!("{e:#}")),
        }
    }
}

/// Start the docs MCP server over stdio.
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let Some(server) = McpDocsServer::from_env()? else {
        anyhow::bail!("SENCLAW_WORKSPACE_STATE_FILE / SENCLAW_DEFAULT_WORKSPACE not set");
    };
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace switched mid-session must move where documents land.
    ///
    /// Capturing the directory at spawn is the tempting shortcut, and it fails
    /// invisibly: the write still succeeds, just into the directory the
    /// session happened to start in.
    #[test]
    fn the_working_directory_is_re_read_on_every_call() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("workspace.json");
        let first = dir.path().join("project-a");
        let second = dir.path().join("project-b");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        let server = McpDocsServer {
            state_file: state.clone(),
            default_workspace: first.clone(),
        };
        // No state file yet: the default workspace stands in.
        assert_eq!(server.working_dir(), first);

        std::fs::write(
            &state,
            serde_json::json!({"currentDir": second.to_string_lossy(), "updatedAt": "now"})
                .to_string(),
        )
        .unwrap();
        assert_eq!(server.working_dir(), second);
        assert_eq!(server.store().root(), second.join("docs"));
    }

    #[test]
    fn a_path_outside_the_project_is_refused_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let server = McpDocsServer {
            state_file: dir.path().join("missing.json"),
            default_workspace: dir.path().to_path_buf(),
        };
        let out = server.store().write("../../escape.md", "# x", false);
        let msg = out.unwrap_err().to_string();
        assert!(msg.contains(".."), "message was: {msg}");
    }

    /// The four tools reach the model under the names the skill and the docs
    /// promise.
    ///
    /// Worth pinning because the failure is invisible from here: a renamed
    /// method silently renames the tool, and the only symptom is an agent
    /// being told to call `doc_write` and finding no such tool mid-turn.
    #[test]
    fn the_four_tools_are_registered_under_their_documented_names() {
        let names: Vec<String> = McpDocsServer::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for expected in ["doc_write", "doc_read", "doc_list", "doc_index_rebuild"] {
            assert!(names.contains(&expected.to_string()), "missing {expected} in {names:?}");
        }
        assert_eq!(names.len(), 4, "unexpected tools: {names:?}");
    }
}

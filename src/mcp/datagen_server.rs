//! Datagen MCP server — `datagen_start`, `datagen_status`, `datagen_cancel`.
//!
//! Like [`crate::mcp::patterns_server`] it owns no state: jobs run in the
//! daemon (`/api/datagen/*`), because a job outlives the tool call that
//! started it and a second registry in this subprocess would never see the
//! first one's jobs.
//!
//! Everything is scoped to the calling chat (`SENCLAW_CHAT_JID`, from this
//! process's environment, never a tool parameter): a job is stamped with it,
//! and status / cancel only reach that chat's jobs and datasets — the rule
//! `watch_stop` follows. `export_logs` from a chat exports that chat only; a
//! group chat must not be able to dump the owner's other conversations.
//!
//! A relative `seeds_file` is resolved against the chat's working folder
//! (the workspace state file), **not** this process's cwd: MCP children are
//! spawned without one, so they inherit the daemon's — on a desktop install
//! the app bundle. With no working folder known, a relative path is refused.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use rmcp::ServiceExt;
use serde_json::{json, Map, Value};

use crate::mcp::schedule_server::ToolResult;
use crate::mcp::workspace_server::WorkspaceServer;

const CALL_TIMEOUT_SECS: u64 = 60;
/// Largest seeds file read; a seed set is examples, not a corpus.
const MAX_SEEDS_FILE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
struct DatagenStartParams {
    /// "expand_seeds" (grow example requests into many new ones, optionally
    /// labelled), "doc_qa" (multi-hop Q&A grounded in documents),
    /// "reasoning_traces" (checked step-by-step solutions to problems),
    /// "executable_qa" (computable problems labelled by running their program
    /// in the sandbox) or "export_logs" (this chat's recorded turns).
    pipeline: String,
    /// Dataset folder name. Default: pipeline + date.
    #[serde(default)]
    name: Option<String>,
    /// expand_seeds / executable_qa: examples — strings, or objects
    /// ({"text","label"} for expand_seeds, {"question","answer"} for executable_qa).
    #[serde(default)]
    seeds: Option<Vec<Value>>,
    /// expand_seeds / executable_qa: a .json (array) or .jsonl file of seeds
    /// instead of `seeds`; relative to the chat's working folder.
    #[serde(default)]
    seeds_file: Option<String>,
    /// doc_qa: documents given inline.
    #[serde(default)]
    texts: Option<Vec<String>>,
    /// doc_qa: .md/.txt files or folders; relative to the chat's working folder.
    #[serde(default)]
    paths: Option<Vec<String>>,
    /// doc_qa: pairs asked per passage (default 3).
    #[serde(default)]
    per_passage: Option<u32>,
    /// doc_qa: require 2+ supporting facts per pair (default true).
    #[serde(default)]
    multi_hop: Option<bool>,
    /// doc_qa: confirm each pair with a separate grounding check (default true).
    #[serde(default)]
    verify: Option<bool>,
    /// reasoning_traces: problems — strings, or {"problem","answer"} objects.
    #[serde(default)]
    problems: Option<Vec<Value>>,
    /// reasoning_traces: a .json/.jsonl file of problems instead of `problems`.
    #[serde(default)]
    problems_file: Option<String>,
    /// reasoning_traces: improvement rounds after the first attempt (default 3).
    #[serde(default)]
    max_iterations: Option<u32>,
    /// reasoning_traces: every judged score must reach this, 0–1 (default 0.7).
    #[serde(default)]
    threshold: Option<f64>,
    /// executable_qa: per-program time limit in ms (default 10000).
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// expand_seeds / executable_qa: items asked for per call.
    #[serde(default)]
    batch: Option<u32>,
    /// expand_seeds / executable_qa: ROUGE-L at which a record counts as a near-copy (default 0.7).
    #[serde(default)]
    max_similarity: Option<f64>,
    /// expand_seeds: description per label, e.g. {"team_dag": "split into parts for several specialists"}.
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    /// expand_seeds: "new" (default) or "evolve" (rewrite seeds harder/more varied).
    #[serde(default)]
    mode: Option<String>,
    /// Records wanted (default 50; reasoning_traces: every problem).
    #[serde(default)]
    target: Option<u32>,
    /// Language to write in; default = the input's language.
    #[serde(default)]
    language: Option<String>,
    /// expand_seeds: keep only records a separate classifier files under the
    /// intended label. Default on with two or more labels.
    #[serde(default)]
    check_label: Option<bool>,
    /// expand_seeds: whole-word phrases a record must not contain.
    #[serde(default)]
    banned_phrases: Option<Vec<String>>,
    /// Hard cap on LLM calls for the job.
    #[serde(default)]
    max_llm_calls: Option<u32>,
    /// LLM config id or label; default = the active model.
    #[serde(default)]
    profile: Option<String>,
    /// export_logs: "sharegpt" (default), "alpaca" or "messages".
    #[serde(default)]
    format: Option<String>,
    /// export_logs: most records to write.
    #[serde(default)]
    max_records: Option<u32>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct DatagenStatusParams {
    /// Job id from datagen_start. Omit to list this chat's recent jobs and datasets.
    #[serde(default)]
    job_id: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct DatagenCancelParams {
    job_id: String,
}

struct Bridge {
    base: String,
    /// Sent as `?owner=` on every read and cancel.
    owner: Option<String>,
}

impl Bridge {
    fn url(&self, path: &str) -> String {
        format!("{}/api/datagen{path}", self.base.trim_end_matches('/'))
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> ToolResult {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(CALL_TIMEOUT_SECS))
            .build()
        {
            Ok(c) => c,
            Err(e) => return ToolResult::err(e.to_string()),
        };
        let url = self.url(path);
        let mut req = client.request(method, &url);
        if let Some(owner) = &self.owner {
            req = req.query(&[("owner", owner)]);
        }
        if let Some((name, value)) = crate::util::internal_auth::header_for(&url) {
            req = req.header(name, value);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("datagen API unreachable: {e}")),
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            return ToolResult::ok(text);
        }
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
            .unwrap_or(text);
        ToolResult::err(detail)
    }
}

#[derive(Clone)]
pub struct McpDatagenServer {
    base: String,
    /// The calling chat: the owner of its jobs, and what `export_logs` exports.
    chat_jid: Option<String>,
    /// The chat's working folder, for a relative `seeds_file`.
    workspace: Option<(PathBuf, PathBuf)>,
}

impl McpDatagenServer {
    /// Build from `SENCLAW_DATAGEN_API_URL`, or `None` when it is absent.
    pub fn from_env() -> Result<Option<Self>> {
        let chat_jid = std::env::var("SENCLAW_CHAT_JID").ok().filter(|v| !v.trim().is_empty());
        let workspace = match (
            std::env::var("SENCLAW_WORKSPACE_STATE_FILE"),
            std::env::var("SENCLAW_DEFAULT_WORKSPACE"),
        ) {
            (Ok(state), Ok(default)) => Some((PathBuf::from(state), PathBuf::from(default))),
            _ => None,
        };
        Ok(std::env::var("SENCLAW_DATAGEN_API_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|base| Self { base, chat_jid, workspace }))
    }

    fn bridge(&self) -> Bridge {
        Bridge { base: self.base.clone(), owner: self.chat_jid.clone() }
    }

    /// The chat's working folder, when this process knows it.
    fn working_dir(&self) -> Option<PathBuf> {
        self.workspace
            .as_ref()
            .map(|(state, default)| WorkspaceServer::new(state, default, None).current_dir())
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpDatagenServer {
    #[rmcp::tool(
        description = "Start a background job that builds a dataset (JSONL + manifest under ~/senclaw/datasets). Pipelines: expand_seeds (example requests → many new varied ones, optionally labelled), doc_qa (documents → multi-hop Q&A grounded in them), reasoning_traces (problems → judged step-by-step solutions), executable_qa (example problems → new ones labelled by running their Python in the sandbox), export_logs (this chat's recorded turns → sharegpt/alpaca/messages, no LLM). Generating pipelines cost LLM calls, capped by max_llm_calls. Returns a job id at once; check it with datagen_status, never by waiting in this turn."
    )]
    async fn datagen_start(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            DatagenStartParams,
        >,
    ) -> String {
        let wd = self.working_dir();
        match build_start_body(p, self.chat_jid.as_deref(), wd.as_deref()) {
            Ok(body) => self.bridge().send(reqwest::Method::POST, "/jobs", Some(body)).await.content,
            Err(e) => ToolResult::err(e).content,
        }
    }

    #[rmcp::tool(
        description = "Progress of a datagen job started from this chat (accepted / target, LLM calls, why candidates were rejected, the dataset folder). Without job_id: this chat's recent jobs and saved datasets."
    )]
    async fn datagen_status(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            DatagenStatusParams,
        >,
    ) -> String {
        match p.job_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => match safe_id(id) {
                Some(id) => self.bridge().send(reqwest::Method::GET, &format!("/jobs/{id}"), None).await.content,
                None => ToolResult::err(format!("\"{id}\" is not a job id")).content,
            },
            None => {
                let jobs = self.bridge().send(reqwest::Method::GET, "/jobs", None).await;
                if jobs.is_error {
                    return jobs.content;
                }
                let datasets = self.bridge().send(reqwest::Method::GET, "/datasets", None).await;
                if datasets.is_error {
                    return datasets.content;
                }
                let field = |r: &ToolResult, k: &str| {
                    serde_json::from_str::<Value>(&r.content).ok().and_then(|v| v.get(k).cloned()).unwrap_or(Value::Null)
                };
                json!({ "jobs": field(&jobs, "jobs"), "datasets": brief(field(&datasets, "datasets")) }).to_string()
            }
        }
    }

    #[rmcp::tool(description = "Stop a running datagen job started from this chat. Records already written stay in the dataset.")]
    async fn datagen_cancel(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            DatagenCancelParams,
        >,
    ) -> String {
        match safe_id(p.job_id.trim()) {
            Some(id) => self.bridge().send(reqwest::Method::POST, &format!("/jobs/{id}/cancel"), None).await.content,
            None => ToolResult::err(format!("\"{}\" is not a job id", p.job_id)).content,
        }
    }
}

/// Job ids are short alphanumerics; anything else would let a value reach
/// another route.
fn safe_id(id: &str) -> Option<&str> {
    (!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric())).then_some(id)
}

/// Dataset manifests without their `params` (every seed of every run), which
/// would flood the model's context for no use in a listing.
fn brief(datasets: Value) -> Value {
    match datasets {
        Value::Array(list) => Value::Array(
            list.into_iter()
                .map(|mut m| {
                    if let Some(o) = m.as_object_mut() {
                        o.remove("params");
                    }
                    m
                })
                .collect(),
        ),
        other => other,
    }
}

/// The REST body for `datagen_start`: the tool's snake_case fields renamed
/// to the API's camelCase, absent ones left out, `seeds_file` read inline,
/// the job stamped with the calling chat, and `export_logs` limited to it.
fn build_start_body(p: DatagenStartParams, chat_jid: Option<&str>, working_dir: Option<&std::path::Path>) -> Result<Value, String> {
    let mut b = Map::new();
    let pipeline = p.pipeline.trim().to_string();
    b.insert("pipeline".into(), json!(pipeline));
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            b.insert(k.into(), v);
        }
    };
    put("owner", chat_jid.map(Value::from));
    put("name", p.name.map(Value::from));
    let seeds = match (p.seeds, p.seeds_file) {
        (Some(_), Some(_)) => return Err("give seeds or seeds_file, not both".into()),
        (Some(s), None) => Some(Value::Array(s)),
        (None, Some(path)) => Some(Value::Array(read_seeds_file(&path, working_dir)?)),
        (None, None) => None,
    };
    put("seeds", seeds);
    let problems = match (p.problems, p.problems_file) {
        (Some(_), Some(_)) => return Err("give problems or problems_file, not both".into()),
        (Some(v), None) => Some(Value::Array(v)),
        (None, Some(path)) => Some(Value::Array(read_seeds_file(&path, working_dir)?)),
        (None, None) => None,
    };
    put("problems", problems);
    put("texts", p.texts.map(|t| json!(t)));
    if let Some(paths) = p.paths {
        // A blank entry would resolve to the working folder itself — the
        // whole of HOME by default — so it is dropped, not resolved.
        let resolved = paths
            .iter()
            .filter(|raw| !raw.trim().is_empty())
            .map(|raw| resolve(raw, working_dir).map(|p| p.to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        put("paths", Some(json!(resolved)));
    }
    put("perPassage", p.per_passage.map(Value::from));
    put("multiHop", p.multi_hop.map(Value::from));
    put("verify", p.verify.map(Value::from));
    put("maxIterations", p.max_iterations.map(Value::from));
    put("threshold", p.threshold.map(Value::from));
    put("timeoutMs", p.timeout_ms.map(Value::from));
    put("batch", p.batch.map(Value::from));
    put("maxSimilarity", p.max_similarity.map(Value::from));
    put("labels", p.labels.map(|l| json!(l)));
    put("mode", p.mode.map(Value::from));
    put("target", p.target.map(Value::from));
    put("language", p.language.map(Value::from));
    put("checkLabel", p.check_label.map(Value::from));
    put("bannedPhrases", p.banned_phrases.map(|v| json!(v)));
    put("maxLlmCalls", p.max_llm_calls.map(Value::from));
    put("profile", p.profile.map(Value::from));
    if pipeline == "export_logs" {
        let chat = chat_jid.ok_or("export_logs needs to know which chat this is; none is configured")?;
        put("chats", Some(json!([chat])));
    }
    put("format", p.format.map(Value::from));
    put("maxRecords", p.max_records.map(Value::from));
    Ok(Value::Object(b))
}

/// A path as the chat means it: relative to its working folder, never to
/// this process's cwd. Refused when relative and no working folder is known.
fn resolve(path: &str, working_dir: Option<&std::path::Path>) -> Result<PathBuf, String> {
    let wd = working_dir.map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    let p = crate::util::paths::resolve_in_workspace(path.trim(), &wd);
    if !p.is_absolute() {
        return Err(format!(
            "\"{path}\" is relative and this chat has no working folder to resolve it against — give an absolute path"
        ));
    }
    Ok(p)
}

/// A JSON array, or one JSON value (string or object) per line.
fn read_seeds_file(path: &str, working_dir: Option<&std::path::Path>) -> Result<Vec<Value>, String> {
    let p = resolve(path, working_dir)?;
    let len = std::fs::metadata(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?.len();
    if len > MAX_SEEDS_FILE_BYTES {
        return Err(format!("{} is larger than {} MB", p.display(), MAX_SEEDS_FILE_BYTES / 1024 / 1024));
    }
    let raw = std::fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
    if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&raw) {
        return Ok(a);
    }
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .enumerate()
        .map(|(i, l)| serde_json::from_str::<Value>(l).map_err(|e| format!("{} line {}: {e}", p.display(), i + 1)))
        .collect()
}

/// Start the Datagen MCP server over stdio. Requires `SENCLAW_DATAGEN_API_URL`.
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpDatagenServer::from_env()
        .inspect_err(|e| tracing::error!("[DatagenMcp] init failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("SENCLAW_DATAGEN_API_URL not set"))?;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_body_renames_fields_and_scopes_to_this_chat() {
        let p = DatagenStartParams {
            pipeline: "export_logs".into(),
            format: Some("alpaca".into()),
            max_records: Some(5),
            ..Default::default()
        };
        let b = build_start_body(p.clone(), Some("web:abc"), None).unwrap();
        assert_eq!(
            b,
            json!({"pipeline": "export_logs", "owner": "web:abc", "chats": ["web:abc"], "format": "alpaca", "maxRecords": 5})
        );
        assert!(build_start_body(p, None, None).unwrap_err().contains("which chat"));

        let p = DatagenStartParams {
            pipeline: "expand_seeds".into(),
            seeds: Some(vec![json!("a b c")]),
            check_label: Some(false),
            max_llm_calls: Some(9),
            ..Default::default()
        };
        let b = build_start_body(p, Some("web:abc"), None).unwrap();
        assert_eq!(
            b,
            json!({"pipeline": "expand_seeds", "owner": "web:abc", "seeds": ["a b c"], "checkLabel": false, "maxLlmCalls": 9})
        );
    }

    #[test]
    fn seeds_file_resolves_against_the_chat_folder_not_the_process_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("s.json"), r#"["one", {"text": "two", "label": "x"}]"#).unwrap();
        assert_eq!(read_seeds_file("s.json", Some(tmp.path())).unwrap().len(), 2);
        let err = read_seeds_file("s.json", None).unwrap_err();
        assert!(err.contains("relative") && err.contains("absolute path"), "{err}");
        let missing = read_seeds_file("nope.json", Some(tmp.path())).unwrap_err();
        assert!(missing.contains(&tmp.path().join("nope.json").display().to_string()), "names the resolved path: {missing}");
    }

    #[test]
    fn seeds_file_reads_arrays_and_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let arr = tmp.path().join("s.json");
        std::fs::write(&arr, r#"["one", {"text": "two", "label": "x"}]"#).unwrap();
        assert_eq!(read_seeds_file(arr.to_str().unwrap(), None).unwrap().len(), 2);
        let lines = tmp.path().join("s.jsonl");
        std::fs::write(&lines, "{\"instruction\": \"a\"}\n\n\"b\"\n").unwrap();
        assert_eq!(read_seeds_file(lines.to_str().unwrap(), None).unwrap(), vec![json!({"instruction": "a"}), json!("b")]);
        std::fs::write(&lines, "not json\n").unwrap();
        assert!(read_seeds_file(lines.to_str().unwrap(), None).unwrap_err().contains("line 1"));

        let p = DatagenStartParams {
            pipeline: "expand_seeds".into(),
            seeds: Some(vec![]),
            seeds_file: Some(arr.to_string_lossy().into()),
            ..Default::default()
        };
        assert!(build_start_body(p, None, None).is_err());
    }

    #[test]
    fn document_paths_resolve_against_the_chat_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let p = DatagenStartParams {
            pipeline: "doc_qa".into(),
            paths: Some(vec!["notes".into(), " ".into(), "/abs/doc.md".into()]),
            verify: Some(false),
            ..Default::default()
        };
        let b = build_start_body(p.clone(), Some("web:abc"), Some(tmp.path())).unwrap();
        assert_eq!(b["paths"], json!([tmp.path().join("notes").to_string_lossy(), "/abs/doc.md"]));
        assert_eq!(b["verify"], false);
        assert!(build_start_body(p, Some("web:abc"), None).unwrap_err().contains("relative"));
    }

    #[test]
    fn listings_drop_manifest_params() {
        let out = brief(json!([{"name": "a", "params": {"seeds": ["x"]}}]));
        assert_eq!(out, json!([{"name": "a"}]));
    }

    #[test]
    fn job_ids_cannot_carry_a_path() {
        assert_eq!(safe_id("a1b2c3d4e5f6"), Some("a1b2c3d4e5f6"));
        assert_eq!(safe_id("../datasets"), None);
        assert_eq!(safe_id(""), None);
    }
}

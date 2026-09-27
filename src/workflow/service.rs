//! WorkflowService — the daemon's workflow coordinator.
//!
//! Port of `SemaClaw/src/workflow/WorkflowService.ts`.
//!
//! Owns the registry + store + executor trio; single entry point for the UI
//! server / WS gateway to trigger and query workflows. Zero coupling with
//! AgentPool / DispatchBridge.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::agent::persona_registry::PersonaRegistry;

use super::executor::{OnUpdate, WorkflowExecutor, WorkflowExecutorOpts};
use super::registry::WorkflowRegistry;
use super::run_store::WorkflowRunStore;
use super::settings::{LiveWorkflowSettings, WorkflowSettings};
use super::types::{StepKind, WorkflowRun};

/// Definition summary for list endpoints / pickers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowDefSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub step_count: usize,
    pub inputs: Vec<WorkflowInputSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub steps: Vec<WorkflowStepSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowInputSummary {
    pub name: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStepSummary {
    pub id: String,
    pub kind: StepKind,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

pub struct WorkflowServiceOpts {
    pub workflows_dir: PathBuf,
    pub workflow_state_path: PathBuf,
    pub workflow_data_dir: PathBuf,
    pub persona_registry: Arc<Mutex<PersonaRegistry>>,
    pub concurrency: Option<usize>,
    /// Extra skills dirs handed to agent-step sessions.
    pub skills_extra_dirs: Vec<String>,
    /// Extra MCP servers injected into agent-step sessions (e.g. browser-mcp).
    pub extra_mcp_servers: Vec<crate::zen_core::McpServerConfig>,
    /// POSIX shell override for script steps.
    pub shell_override: Option<String>,
    /// Fired after each persisted state change (daemon → WS push).
    pub on_update: Option<OnUpdate>,
}

pub struct WorkflowService {
    registry: Mutex<WorkflowRegistry>,
    store: Arc<WorkflowRunStore>,
    executor: Arc<WorkflowExecutor>,
    workflows_dir: PathBuf,
    persona_registry: Arc<Mutex<PersonaRegistry>>,
    settings_path: PathBuf,
    live_settings: LiveWorkflowSettings,
}

impl WorkflowService {
    pub fn new(opts: WorkflowServiceOpts) -> Self {
        let workflows_dir = opts.workflows_dir.clone();
        let persona_registry = Arc::clone(&opts.persona_registry);
        // Runtime settings live next to the run store and apply live via
        // shared atomics (LLM parallelism, no-result retries).
        let settings_path = opts
            .workflow_state_path
            .with_file_name("workflow-settings.json");
        let live_settings = LiveWorkflowSettings::new(&WorkflowSettings::load(&settings_path));
        let registry = Mutex::new(WorkflowRegistry::new(opts.workflows_dir));
        let store = Arc::new(WorkflowRunStore::new(opts.workflow_state_path));
        // Startup reconciliation: `running` orphans left by a previous
        // process will never progress — mark them interrupted.
        let orphaned = store.reconcile_orphans();
        if orphaned > 0 {
            tracing::warn!(
                "[WorkflowService] reconciled {orphaned} interrupted run(s) left running by a previous process"
            );
        }
        let executor = WorkflowExecutor::new(WorkflowExecutorOpts {
            store: Arc::clone(&store),
            persona_registry: opts.persona_registry,
            concurrency: opts.concurrency,
            on_update: opts.on_update,
            workflow_data_dir: opts.workflow_data_dir,
            skills_extra_dirs: opts.skills_extra_dirs,
            extra_mcp_servers: opts.extra_mcp_servers,
            shell_override: opts.shell_override,
            settings: live_settings.clone(),
        });
        Self {
            registry,
            store,
            executor,
            workflows_dir,
            persona_registry,
            settings_path,
            live_settings,
        }
    }

    /// Current runtime settings (snapshot of the live atomics).
    pub fn get_settings(&self) -> WorkflowSettings {
        self.live_settings.snapshot()
    }

    /// Persist + apply new settings; takes effect immediately (queued agent
    /// steps see the new parallelism on the next dispatch).
    pub fn set_settings(&self, s: WorkflowSettings) -> Result<WorkflowSettings, String> {
        let s = s.clamped();
        s.save(&self.settings_path)
            .map_err(|e| format!("save settings failed: {e}"))?;
        self.live_settings.apply(&s);
        Ok(s)
    }

    // ===== Definition CRUD (files in workflows_dir) =====

    /// Raw markdown of a definition, for edit/export. Returns
    /// `(file_name, content)`.
    pub fn get_definition(&self, name: &str) -> Option<(String, String)> {
        let path = {
            let mut reg = self.registry.lock().unwrap();
            reg.reload();
            reg.get(name).map(|d| d.file_path.clone())
        }?;
        let content = std::fs::read_to_string(&path).ok()?;
        let file_name = path.file_name()?.to_string_lossy().to_string();
        Some((file_name, content))
    }

    /// Create a new definition (also the import path). Validates the content
    /// before writing. Fails if a workflow with the same name already exists
    /// unless `overwrite` is set. Returns the parsed workflow name.
    pub fn create_definition(&self, content: &str, overwrite: bool) -> Result<String, String> {
        // Validate against the target path the file will get.
        let probe =
            super::registry::parse_workflow_source(content, &self.workflows_dir.join("new.md"))
                .map_err(|e| format!("invalid workflow definition: {e:#}"))?;

        let mut reg = self.registry.lock().unwrap();
        reg.reload();
        let existing = reg.get(&probe.name).map(|d| d.file_path.clone());
        let target = match existing {
            Some(_) if !overwrite => {
                return Err(format!("workflow \"{}\" already exists", probe.name))
            }
            Some(path) => path,
            None => {
                let base = super::types::sanitize_name(&probe.name);
                let mut path = self.workflows_dir.join(format!("{base}.md"));
                // Name is free but the file slot may be taken by another
                // workflow (custom `name:` ≠ file stem) — pick a free slot.
                let mut n = 2;
                while path.exists() {
                    path = self.workflows_dir.join(format!("{base}-{n}.md"));
                    n += 1;
                }
                path
            }
        };
        write_atomic(&target, content).map_err(|e| format!("write failed: {e}"))?;
        reg.reload();
        Ok(probe.name)
    }

    /// Overwrite an existing definition's file with new content (edit path).
    /// Validates first; the new content may rename the workflow.
    pub fn update_definition(&self, name: &str, content: &str) -> Result<String, String> {
        let mut reg = self.registry.lock().unwrap();
        reg.reload();
        let Some(path) = reg.get(name).map(|d| d.file_path.clone()) else {
            return Err(format!("workflow \"{name}\" not found"));
        };
        let probe = super::registry::parse_workflow_source(content, &path)
            .map_err(|e| format!("invalid workflow definition: {e:#}"))?;
        write_atomic(&path, content).map_err(|e| format!("write failed: {e}"))?;
        reg.reload();
        Ok(probe.name)
    }

    /// Generate a draft workflow definition from a natural-language
    /// description via a one-shot agent. Never touches disk — the caller
    /// shows the draft to the user, who saves it through
    /// `create_definition`. Validates the output and retries once with the
    /// parse error appended so the agent can self-correct.
    pub async fn draft_definition(&self, description: &str) -> Result<(String, String), String> {
        let description = description.trim();
        if description.is_empty() {
            return Err("description is empty".to_string());
        }
        let personas: Vec<(String, String)> = {
            let reg = self.persona_registry.lock().unwrap();
            reg.list()
                .iter()
                .map(|p| (p.name.clone(), p.description.clone()))
                .collect()
        };
        let base_prompt = build_draft_prompt(description, &personas);
        let probe_path = self.workflows_dir.join("draft.md");
        let working_dir = self.workflows_dir.to_string_lossy().to_string();

        let mut prompt = base_prompt.clone();
        let mut last_err = String::new();
        for attempt in 0..2 {
            let res = crate::agent::isolated_runner::run_one_shot(
                crate::agent::isolated_runner::OneShotOptions {
                    prompt: prompt.clone(),
                    working_dir: working_dir.clone(),
                    instance_id: Some(format!(
                        "wf-draft-{:x}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    )),
                    // Text-only generation: no file writes, no shell.
                    use_tools: vec!["Read".to_string()],
                    timeout: Some(std::time::Duration::from_secs(180)),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| format!("draft agent failed: {e:#}"))?;
            if res.timed_out {
                return Err("draft agent timed out after 180s".to_string());
            }
            if res.errored {
                last_err = format!(
                    "agent session error: {}",
                    res.error_message.as_deref().unwrap_or("unknown")
                );
                tracing::warn!("[WorkflowService] draft attempt {attempt}: {last_err}");
                if attempt == 0 {
                    continue; // transient LLM errors deserve one retry
                }
                return Err(format!("draft failed: {last_err}"));
            }

            // The document may be in an earlier turn (models often close with
            // a remark) — scan the turns newest-first.
            let content = res
                .all_texts
                .iter()
                .rev()
                .map(|t| extract_markdown_draft(t))
                .find(|c| !c.trim().is_empty())
                .unwrap_or_default();

            if content.trim().is_empty() {
                let preview: String = res.text.chars().take(300).collect();
                tracing::warn!(
                    "[WorkflowService] draft attempt {attempt}: no document in reply \
                     (turns={}, preview: {preview:?})",
                    res.all_texts.len()
                );
                last_err = if res.text.trim().is_empty() {
                    "agent returned an empty reply (check the model/LLM config)".to_string()
                } else {
                    format!(
                        "agent returned no workflow definition (reply started with: {:?})",
                        res.text.chars().take(120).collect::<String>()
                    )
                };
                if attempt == 0 {
                    prompt = format!(
                        "{base_prompt}\n\n---\nYour previous reply contained no workflow \
                         document. Reply with ONLY the markdown document — the very first \
                         line of your reply must be `---`."
                    );
                    continue;
                }
            } else {
                match super::registry::parse_workflow_source(&content, &probe_path) {
                    Ok(def) => return Ok((content, def.name)),
                    Err(e) => last_err = format!("{e:#}"),
                }
                tracing::warn!(
                    "[WorkflowService] draft attempt {attempt} failed validation: {last_err}"
                );
                if attempt == 0 {
                    prompt = format!(
                        "{base_prompt}\n\n---\nYour previous attempt FAILED validation with this \
                         error:\n  {last_err}\n\nPrevious attempt:\n{content}\n\nFix the problem \
                         and output the corrected COMPLETE markdown document only."
                    );
                    continue;
                }
            }
        }
        Err(format!("draft failed validation: {last_err}"))
    }

    /// Targeted field edit (the "tune guidance" form): workflow-level
    /// guidance plus per-step guidance/timeout. Only the YAML frontmatter is
    /// rewritten (via serde_yaml — comments/key order in the frontmatter may
    /// normalize); the markdown body is preserved byte-for-byte. Empty
    /// guidance strings remove the field.
    pub fn edit_definition_fields(&self, name: &str, patch: &DefFieldsPatch) -> Result<(), String> {
        let path = {
            let mut reg = self.registry.lock().unwrap();
            reg.reload();
            let Some(p) = reg.get(name).map(|d| d.file_path.clone()) else {
                return Err(format!("workflow \"{name}\" not found"));
            };
            p
        };
        let raw = std::fs::read_to_string(&path).map_err(|e| format!("read failed: {e}"))?;
        let rebuilt = apply_def_fields_patch(&raw, patch)?;
        // Full validation before touching disk — a bad patch can't corrupt the file.
        super::registry::parse_workflow_source(&rebuilt, &path)
            .map_err(|e| format!("patched definition is invalid: {e:#}"))?;
        write_atomic(&path, &rebuilt).map_err(|e| format!("write failed: {e}"))?;
        self.registry.lock().unwrap().reload();
        Ok(())
    }

    /// Delete a definition file. Run history and the workspace dir are kept.
    pub fn delete_definition(&self, name: &str) -> Result<(), String> {
        let mut reg = self.registry.lock().unwrap();
        reg.reload();
        let Some(path) = reg.get(name).map(|d| d.file_path.clone()) else {
            return Err(format!("workflow \"{name}\" not found"));
        };
        std::fs::remove_file(&path).map_err(|e| format!("delete failed: {e}"))?;
        reg.reload();
        Ok(())
    }

    /// Available workflow definitions (summaries). Re-scans on every call so
    /// agent/CLI-created workflows show up without a restart.
    pub fn list_defs(&self) -> Vec<WorkflowDefSummary> {
        let mut reg = self.registry.lock().unwrap();
        reg.reload();
        reg.list()
            .into_iter()
            .map(|d| WorkflowDefSummary {
                name: d.name.clone(),
                description: d.description.clone(),
                step_count: d.steps.len(),
                inputs: d
                    .inputs
                    .iter()
                    .map(|i| WorkflowInputSummary {
                        name: i.name.clone(),
                        required: i.required,
                        default: i.default.clone(),
                        description: i.description.clone(),
                    })
                    .collect(),
                guidance: d.guidance.clone(),
                workspace: d.workspace.clone(),
                steps: d
                    .steps
                    .iter()
                    .map(|s| WorkflowStepSummary {
                        id: s.id.clone(),
                        kind: s.kind,
                        depends_on: s.depends_on.clone(),
                        persona: s.persona.clone(),
                        guidance: s.guidance.clone(),
                        timeout: s.timeout,
                    })
                    .collect(),
            })
            .collect()
    }

    /// All run records (newest first).
    pub fn list_runs(&self) -> Vec<WorkflowRun> {
        self.store.load()
    }

    pub fn get_run(&self, id: &str) -> Option<WorkflowRun> {
        self.store.get(id)
    }

    /// Live-activity feed of a run (thinking / tool calls / messages).
    pub fn run_activity(&self, id: &str) -> Vec<super::executor::ActivityEntry> {
        self.executor.run_activity(id)
    }

    /// Fire-and-forget run trigger. Returns the run id synchronously; state
    /// updates flow through `on_update`.
    pub fn start_run(
        &self,
        name: &str,
        inputs: HashMap<String, String>,
        trigger: Option<String>,
    ) -> Result<String, String> {
        let def = {
            let mut reg = self.registry.lock().unwrap();
            reg.reload(); // pick up freshly authored workflows
            reg.get(name).cloned()
        };
        let Some(def) = def else {
            return Err(format!("workflow \"{name}\" not found or invalid"));
        };
        match self.executor.start(&def, inputs, trigger) {
            Ok((run, _handle)) => Ok(run.id),
            Err(e) => Err(format!("{e:#}")),
        }
    }

    /// Cancel an active run (stop dispatching, abort in-flight steps).
    pub fn cancel(&self, run_id: &str) -> bool {
        self.executor.cancel(run_id)
    }

    /// Rename a run (display label; empty clears).
    pub fn rename_run(&self, run_id: &str, label: &str) -> Result<(), String> {
        self.store.rename_run(run_id, label)
    }

    /// Delete a run record. Active runs must be cancelled first — the
    /// executor would just re-persist them on its next state change.
    pub fn delete_run(&self, run_id: &str) -> Result<(), String> {
        if self.executor.is_running(run_id) {
            return Err(format!(
                "run \"{run_id}\" is still running — cancel it first"
            ));
        }
        self.store.delete_run(run_id)
    }

    pub fn is_running(&self, run_id: &str) -> bool {
        self.executor.is_running(run_id)
    }

    /// Shutdown: flush pending run state to disk.
    pub fn flush(&self) {
        self.store.flush();
    }
}

/// Targeted-edit payload for the "tune guidance" form.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct DefFieldsPatch {
    /// Workflow-level guidance. `None` = untouched; `Some("")` = remove.
    #[serde(default)]
    pub guidance: Option<String>,
    /// Workflow-level workspace path. `None` = untouched; `Some("")` = remove
    /// (falls back to the default per-workflow directory).
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub steps: Vec<StepFieldsPatch>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct StepFieldsPatch {
    pub id: String,
    /// `None` = untouched; `Some("")` = remove.
    #[serde(default)]
    pub guidance: Option<String>,
    /// `None` = untouched. Must be > 0.
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// Splice a [`DefFieldsPatch`] into the raw markdown's YAML frontmatter,
/// leaving everything outside the frontmatter untouched.
fn apply_def_fields_patch(raw: &str, patch: &DefFieldsPatch) -> Result<String, String> {
    let lines: Vec<&str> = raw.lines().collect();
    let mut start = None;
    let mut end = None;
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "---" {
            if start.is_none() {
                start = Some(i);
            } else {
                end = Some(i);
                break;
            }
        }
    }
    let (s, e) = match (start, end) {
        (Some(s), Some(e)) => (s, e),
        _ => return Err("no frontmatter to edit".to_string()),
    };
    let fm_text = lines[s + 1..e].join("\n");
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(&fm_text).map_err(|err| format!("frontmatter parse: {err}"))?;
    let map = doc
        .as_mapping_mut()
        .ok_or_else(|| "frontmatter is not a mapping".to_string())?;

    set_or_remove_str(map, "guidance", patch.guidance.as_deref());
    set_or_remove_str(map, "workspace", patch.workspace.as_deref());

    if !patch.steps.is_empty() {
        let steps = map
            .get_mut(serde_yaml::Value::from("steps"))
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "no steps in definition".to_string())?;
        for sp in &patch.steps {
            let step = steps
                .iter_mut()
                .filter_map(|v| v.as_mapping_mut())
                .find(|m| {
                    m.get(serde_yaml::Value::from("id"))
                        .and_then(|v| v.as_str())
                        == Some(sp.id.as_str())
                })
                .ok_or_else(|| format!("step \"{}\" not found", sp.id))?;
            set_or_remove_str(step, "guidance", sp.guidance.as_deref());
            if let Some(t) = sp.timeout {
                if t == 0 {
                    return Err(format!("step \"{}\": timeout must be > 0", sp.id));
                }
                step.insert(
                    serde_yaml::Value::from("timeout"),
                    serde_yaml::Value::from(t),
                );
            }
        }
    }

    let mut new_fm = serde_yaml::to_string(&doc).map_err(|err| format!("serialize: {err}"))?;
    // serde_yaml may or may not emit a leading document marker; normalize it
    // away since we re-wrap with our own `---` fences.
    if let Some(stripped) = new_fm.strip_prefix("---\n") {
        new_fm = stripped.to_string();
    }
    let new_fm = new_fm.trim_end_matches('\n');

    let mut out: Vec<&str> = Vec::new();
    out.extend_from_slice(&lines[..=s]);
    out.extend(new_fm.lines());
    out.extend_from_slice(&lines[e..]);
    let mut rebuilt = out.join("\n");
    if raw.ends_with('\n') && !rebuilt.ends_with('\n') {
        rebuilt.push('\n');
    }
    Ok(rebuilt)
}

/// `None` = untouched; `Some(blank)` = remove key; `Some(text)` = set.
fn set_or_remove_str(map: &mut serde_yaml::Mapping, key: &str, value: Option<&str>) {
    let Some(value) = value else { return };
    let k = serde_yaml::Value::from(key);
    if value.trim().is_empty() {
        map.remove(&k);
    } else {
        map.insert(k, serde_yaml::Value::from(value));
    }
}

/// Authoring prompt for the draft agent: condensed definition rules + the
/// personas that actually exist (agent steps referencing anything else fail
/// at runtime).
fn build_draft_prompt(description: &str, personas: &[(String, String)]) -> String {
    let persona_list = if personas.is_empty() {
        "  (none — use ONLY script steps; do not invent personas)".to_string()
    } else {
        personas
            .iter()
            .map(|(n, d)| format!("  - {n} — {d}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        r#"You author SenClaw workflow definitions: a markdown file whose YAML frontmatter declares a DAG of steps.

Rules:
- Frontmatter fields: name (kebab-case, unique), description, inputs (list of {{ name, required?, default? }}), guidance (workflow-level rules applied to ALL agent steps), steps.
- Each step: id (snake_case, unique), kind: agent|script.
  - agent steps: persona (MUST be from the list below), prompt (the task; may interpolate {{{{input.X}}}} and {{{{steps.ID.result}}}}), guidance (stable rules: output format, scope, tone), timeout (seconds, default 600).
  - script steps: run (POSIX shell; read values from env $WF_INPUT_<NAME> / $WF_STEP_<ID>_RESULT — never interpolate {{{{}}}} into shell), for deterministic work only (fetch/transform/files).
- Referencing {{{{steps.X.result}}}} or $WF_STEP_X_RESULT auto-creates the dependency; write dependsOn only for ordering-only dependencies.
- Fan-out pattern: N parallel sibling agent steps + one aggregator that references all their results.
- Give every agent step a sensible guidance — that is the field the user will tune afterwards.
- Agent steps that browse the web or do multi-source research are slow: give them `timeout: 900`.
- Add observe: {{ label, from: result, as: inline }} on steps whose output a human wants to glance at.
- Write description/prompts/guidance/labels in the same language as the user's description below.

Available personas (agent steps may ONLY use these):
{persona_list}

User's description of the routine:
{description}

Output ONLY the complete markdown document (starting with `---`). No explanations before or after."#
    )
}

/// Pull the workflow markdown out of the agent's reply: prefer a fenced block
/// whose first line is `---`; otherwise take everything from the first `---`
/// line onward.
fn extract_markdown_draft(text: &str) -> String {
    // Fenced block whose body starts with the frontmatter marker.
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(after.len());
        let body = &after[body_start..];
        if let Some(close) = body.find("```") {
            let candidate = body[..close].trim();
            if candidate.starts_with("---") {
                return candidate.to_string();
            }
            rest = &body[close + 3..];
        } else {
            break;
        }
    }
    // Bare document: from the first `---` line to the end.
    let mut offset = 0;
    for line in text.lines() {
        if line.trim() == "---" {
            return text[offset..].trim().to_string();
        }
        offset += line.len() + 1;
    }
    String::new()
}

/// tmp+rename atomic write (never leaves a half-written definition).
fn write_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(dir: &std::path::Path) -> WorkflowService {
        WorkflowService::new(WorkflowServiceOpts {
            workflows_dir: dir.join("workflows"),
            workflow_state_path: dir.join("workflow-runs.json"),
            workflow_data_dir: dir.join("workflow-data"),
            persona_registry: Arc::new(Mutex::new(PersonaRegistry::new(dir.join("personas")))),
            concurrency: None,
            skills_extra_dirs: vec![],
            extra_mcp_servers: vec![],
            shell_override: None,
            on_update: None,
        })
    }

    #[tokio::test]
    async fn list_defs_picks_up_new_files_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        assert!(svc.list_defs().is_empty());

        std::fs::write(
            dir.path().join("workflows").join("hello.md"),
            "---\nsteps:\n  - { id: a, kind: script, run: echo hi }\n---\n",
        )
        .unwrap();
        let defs = svc.list_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "hello");
        assert_eq!(defs[0].step_count, 1);
    }

    #[tokio::test]
    async fn start_run_returns_id_and_completes() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        std::fs::write(
            dir.path().join("workflows").join("quick.md"),
            "---\nsteps:\n  - { id: a, kind: script, run: echo ok }\n---\n",
        )
        .unwrap();
        let run_id = svc
            .start_run("quick", HashMap::new(), Some("test".into()))
            .unwrap();
        assert_eq!(run_id, "quick-0001");

        // Poll until the background task finishes.
        for _ in 0..100 {
            if !svc.is_running(&run_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let run = svc.get_run(&run_id).unwrap();
        assert_eq!(run.status, super::super::types::RunStatus::Done);
        assert_eq!(run.steps[0].result, "ok");
    }

    #[tokio::test]
    async fn start_unknown_workflow_errors() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        let err = svc.start_run("ghost", HashMap::new(), None).unwrap_err();
        assert!(err.contains("not found"));
    }

    const DEF: &str =
        "---\nname: crud-wf\nsteps:\n  - { id: a, kind: script, run: echo 1 }\n---\nbody\n";

    #[tokio::test]
    async fn definition_crud_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());

        // create (import)
        let name = svc.create_definition(DEF, false).unwrap();
        assert_eq!(name, "crud-wf");
        assert!(dir.path().join("workflows").join("crud-wf.md").exists());
        // duplicate rejected without overwrite
        assert!(svc
            .create_definition(DEF, false)
            .unwrap_err()
            .contains("already exists"));
        // overwrite allowed
        svc.create_definition(DEF, true).unwrap();

        // get (export)
        let (file_name, content) = svc.get_definition("crud-wf").unwrap();
        assert_eq!(file_name, "crud-wf.md");
        assert_eq!(content, DEF);

        // update (edit) — invalid content rejected, file untouched
        let err = svc
            .update_definition("crud-wf", "no frontmatter")
            .unwrap_err();
        assert!(err.contains("invalid workflow definition"), "{err}");
        assert_eq!(svc.get_definition("crud-wf").unwrap().1, DEF);
        // valid update goes through
        let updated = DEF.replace("echo 1", "echo 2");
        svc.update_definition("crud-wf", &updated).unwrap();
        assert!(svc.get_definition("crud-wf").unwrap().1.contains("echo 2"));

        // delete
        svc.delete_definition("crud-wf").unwrap();
        assert!(svc.get_definition("crud-wf").is_none());
        assert!(svc.delete_definition("crud-wf").is_err());
    }

    const TUNABLE: &str = r#"---
name: tune-me
description: giữ nguyên
guidance: luật cũ
steps:
  - id: research
    kind: agent
    persona: researcher
    prompt: "làm {{input.x}}"
    guidance: cũ
  - id: fetch
    kind: script
    run: echo hi
---
Thân bài giữ nguyên từng byte.
"#;

    #[tokio::test]
    async fn edit_fields_updates_guidance_and_timeout_preserving_body() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        svc.create_definition(TUNABLE, false).unwrap();

        svc.edit_definition_fields(
            "tune-me",
            &DefFieldsPatch {
                guidance: Some("luật mới\nnhiều dòng".to_string()),
                workspace: None,
                steps: vec![StepFieldsPatch {
                    id: "research".to_string(),
                    guidance: Some("guidance step mới".to_string()),
                    timeout: Some(300),
                }],
            },
        )
        .unwrap();

        let (_, content) = svc.get_definition("tune-me").unwrap();
        assert!(content.contains("Thân bài giữ nguyên từng byte."));
        assert!(content.contains("luật mới"));
        assert!(content.contains("guidance step mới"));
        // Re-parse reflects the change.
        let defs = svc.list_defs();
        let d = defs.iter().find(|d| d.name == "tune-me").unwrap();
        assert_eq!(d.guidance.as_deref(), Some("luật mới\nnhiều dòng"));
        let step = d.steps.iter().find(|s| s.id == "research").unwrap();
        assert_eq!(step.guidance.as_deref(), Some("guidance step mới"));
        assert_eq!(step.timeout, Some(300));

        // Empty string removes guidance.
        svc.edit_definition_fields(
            "tune-me",
            &DefFieldsPatch {
                guidance: Some(String::new()),
                workspace: None,
                steps: vec![],
            },
        )
        .unwrap();
        let defs = svc.list_defs();
        let d = defs.iter().find(|d| d.name == "tune-me").unwrap();
        assert!(d.guidance.is_none());
    }

    #[tokio::test]
    async fn edit_fields_updates_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        svc.create_definition(TUNABLE, false).unwrap();

        svc.edit_definition_fields(
            "tune-me",
            &DefFieldsPatch {
                guidance: None,
                workspace: Some("/tmp/wf-ws".to_string()),
                steps: vec![],
            },
        )
        .unwrap();
        let defs = svc.list_defs();
        let d = defs.iter().find(|d| d.name == "tune-me").unwrap();
        assert_eq!(d.workspace.as_deref(), Some("/tmp/wf-ws"));

        // Empty string removes it (falls back to the default directory).
        svc.edit_definition_fields(
            "tune-me",
            &DefFieldsPatch {
                guidance: None,
                workspace: Some(String::new()),
                steps: vec![],
            },
        )
        .unwrap();
        let defs = svc.list_defs();
        let d = defs.iter().find(|d| d.name == "tune-me").unwrap();
        assert!(d.workspace.is_none());
    }

    #[tokio::test]
    async fn edit_fields_rejects_unknown_step_and_zero_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        svc.create_definition(TUNABLE, false).unwrap();

        let err = svc
            .edit_definition_fields(
                "tune-me",
                &DefFieldsPatch {
                    guidance: None,
                    workspace: None,
                    steps: vec![StepFieldsPatch {
                        id: "ghost".to_string(),
                        guidance: Some("x".to_string()),
                        timeout: None,
                    }],
                },
            )
            .unwrap_err();
        assert!(err.contains("not found"), "{err}");

        let err = svc
            .edit_definition_fields(
                "tune-me",
                &DefFieldsPatch {
                    guidance: None,
                    workspace: None,
                    steps: vec![StepFieldsPatch {
                        id: "fetch".to_string(),
                        guidance: None,
                        timeout: Some(0),
                    }],
                },
            )
            .unwrap_err();
        assert!(err.contains("timeout"), "{err}");
        // File untouched by failed patches.
        assert_eq!(svc.get_definition("tune-me").unwrap().1, TUNABLE);
    }

    #[test]
    fn extract_draft_handles_fenced_and_bare() {
        let fenced = "Here you go:\n```yaml\n---\nname: a\n---\nbody\n```\nHope it helps!";
        assert_eq!(extract_markdown_draft(fenced), "---\nname: a\n---\nbody");

        let bare = "Sure!\n---\nname: b\n---\n";
        assert!(extract_markdown_draft(bare).starts_with("---\nname: b"));

        assert_eq!(extract_markdown_draft("no document here"), "");
    }

    #[test]
    fn draft_prompt_lists_personas_or_forbids_agents() {
        let p = build_draft_prompt("làm x", &[("researcher".into(), "tra cứu".into())]);
        assert!(p.contains("- researcher — tra cứu"));
        assert!(p.contains("làm x"));
        let p2 = build_draft_prompt("làm x", &[]);
        assert!(p2.contains("ONLY script steps"));
    }

    #[tokio::test]
    async fn create_definition_avoids_taken_file_slot() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        // A file whose stem clashes but whose `name:` differs.
        std::fs::create_dir_all(dir.path().join("workflows")).unwrap();
        std::fs::write(
            dir.path().join("workflows").join("crud-wf.md"),
            "---\nname: other\nsteps:\n  - { id: a, kind: script, run: echo x }\n---\n",
        )
        .unwrap();
        let name = svc.create_definition(DEF, false).unwrap();
        assert_eq!(name, "crud-wf");
        assert!(dir.path().join("workflows").join("crud-wf-2.md").exists());
        // Both workflows visible.
        assert_eq!(svc.list_defs().len(), 2);
    }
}

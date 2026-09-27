//! WorkflowRegistry — load + validate workflow definitions.
//!
//! Port of `SemaClaw/src/workflow/WorkflowRegistry.ts`.
//!
//! Scans `<workflows_dir>/*.md`, parses the YAML frontmatter (nested
//! steps/inputs/observe), validates the DAG (unique ids, existing dependsOn,
//! acyclic, valid kind, agent needs persona, script needs run/scriptFile).
//! Invalid files are skipped with a warning (they don't poison the list).
//!
//! Hot reload: callers (`WorkflowService`) re-scan on each list/start, so no
//! filesystem watcher is needed here (matches upstream semantics).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use super::template::{env_segment, extract_template_step_refs};
use super::types::{
    ObserveAs, ObserveFrom, ObserveSpec, StepKind, WorkflowDef, WorkflowInput, WorkflowStep,
};

pub struct WorkflowRegistry {
    dir: PathBuf,
    workflows: HashMap<String, WorkflowDef>,
}

impl WorkflowRegistry {
    pub fn new(dir: PathBuf) -> Self {
        // Ensure the dir exists so workflows created later by agent/CLI are found.
        let _ = std::fs::create_dir_all(&dir);
        let mut reg = Self {
            dir,
            workflows: HashMap::new(),
        };
        reg.load_all();
        reg
    }

    pub fn get(&self, name: &str) -> Option<&WorkflowDef> {
        self.workflows.get(name)
    }

    pub fn list(&self) -> Vec<&WorkflowDef> {
        let mut defs: Vec<&WorkflowDef> = self.workflows.values().collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    pub fn reload(&mut self) {
        self.load_all();
    }

    fn load_all(&mut self) {
        let mut map = HashMap::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(_) => {
                self.workflows = map;
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            match parse_workflow_file(&path) {
                Ok(def) => {
                    map.insert(def.name.clone(), def);
                }
                Err(e) => {
                    tracing::warn!(
                        "[WorkflowRegistry] Skip {}: {e:#}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
        }
        self.workflows = map;
        tracing::debug!(
            "[WorkflowRegistry] Loaded {} workflow(s)",
            self.workflows.len()
        );
    }
}

/// Parse a single `.md` into a `WorkflowDef`; invalid files error.
pub fn parse_workflow_file(file_path: &Path) -> Result<WorkflowDef> {
    let raw = std::fs::read_to_string(file_path)
        .with_context(|| format!("read {}", file_path.display()))?;
    parse_workflow_source(&raw, file_path)
}

/// Parse workflow markdown source against a (possibly not-yet-existing) file
/// path. Used both by file loading and by the definition-save API to validate
/// content before writing it to disk.
pub fn parse_workflow_source(raw: &str, file_path: &Path) -> Result<WorkflowDef> {
    let fm_text = extract_frontmatter(raw).ok_or_else(|| anyhow!("no frontmatter"))?;

    let parsed: serde_yaml::Value =
        serde_yaml::from_str(&fm_text).context("frontmatter is not valid YAML")?;
    if !parsed.is_mapping() {
        bail!("frontmatter is not a mapping");
    }

    normalize_and_validate(&parsed, file_path)
}

/// Extract the text between the first `---` ... `---` pair.
fn extract_frontmatter(raw: &str) -> Option<String> {
    let mut start = None;
    let mut end = None;
    for (i, line) in raw.lines().enumerate() {
        if line.trim() == "---" {
            if start.is_none() {
                start = Some(i);
            } else {
                end = Some(i);
                break;
            }
        }
    }
    let (s, e) = (start?, end?);
    let lines: Vec<&str> = raw.lines().collect();
    Some(lines[s + 1..e].join("\n"))
}

fn str_field(v: &serde_yaml::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn normalize_and_validate(fm: &serde_yaml::Value, file_path: &Path) -> Result<WorkflowDef> {
    let file_name = file_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = str_field(fm, "name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(file_name);

    let raw_steps = fm
        .get("steps")
        .and_then(|s| s.as_sequence())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("steps must be a non-empty array"))?;

    let mut ids: HashSet<String> = HashSet::new();
    let mut steps: Vec<WorkflowStep> = raw_steps
        .iter()
        .enumerate()
        .map(|(i, s)| normalize_step(s, i, &mut ids))
        .collect::<Result<_>>()?;

    // Data references → implicit dependencies: fold `{{steps.X.result}}` /
    // `$WF_STEP_X_RESULT` references into depends_on so a reference and its
    // dependency can never drift apart (otherwise a referenced step might not
    // have run yet and would render as an empty string).
    infer_dependencies(&mut steps, &ids)?;

    // Every depends_on target must exist (including inferred ones — typos
    // fail loud here instead of silently rendering empty).
    for s in &steps {
        for d in &s.depends_on {
            if !ids.contains(d) {
                bail!("step \"{}\" dependsOn unknown step \"{}\"", s.id, d);
            }
        }
    }
    assert_acyclic(&steps)?;

    // Workflow-level guidance is folded into every agent step; a step ref
    // there would force every agent step to depend on it (guaranteed cycle).
    let guidance = str_field(fm, "guidance");
    if !extract_template_step_refs(guidance.as_deref()).is_empty() {
        bail!(
            "workflow-level guidance must not reference {{{{steps.*.result}}}} (it applies to \
             every agent step); move step-dependent text into a step's prompt/guidance"
        );
    }

    Ok(WorkflowDef {
        name,
        description: str_field(fm, "description"),
        version: str_field(fm, "version"),
        inputs: normalize_inputs(fm.get("inputs"))?,
        guidance,
        workspace: str_field(fm, "workspace"),
        steps,
        file_path: file_path.to_path_buf(),
        source: "user".to_string(),
    })
}

fn normalize_step(
    raw: &serde_yaml::Value,
    idx: usize,
    ids: &mut HashSet<String>,
) -> Result<WorkflowStep> {
    if !raw.is_mapping() {
        bail!("step[{idx}] is not a mapping");
    }

    let id = str_field(raw, "id").ok_or_else(|| anyhow!("step[{idx}] missing id"))?;
    if !ids.insert(id.clone()) {
        bail!("duplicate step id \"{id}\"");
    }

    let kind = match str_field(raw, "kind").as_deref() {
        Some("agent") => StepKind::Agent,
        Some("script") => StepKind::Script,
        _ => bail!("step \"{id}\" kind must be agent|script"),
    };

    let depends_on: Vec<String> = raw
        .get("dependsOn")
        .and_then(|d| d.as_sequence())
        .map(|seq| {
            seq.iter()
                .map(|v| match v.as_str() {
                    Some(s) => s.to_string(),
                    None => serde_yaml::to_string(v)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let timeout = raw.get("timeout").and_then(|t| t.as_u64());

    let mut step = WorkflowStep {
        id: id.clone(),
        kind,
        depends_on,
        timeout,
        guidance: str_field(raw, "guidance"),
        observe: normalize_observe(raw.get("observe"), &id)?,
        persona: None,
        prompt: None,
        run: None,
        script_file: None,
    };

    match kind {
        StepKind::Agent => {
            step.persona = str_field(raw, "persona");
            step.prompt = str_field(raw, "prompt");
            if step.persona.is_none() {
                bail!("agent step \"{id}\" missing persona");
            }
        }
        StepKind::Script => {
            step.run = str_field(raw, "run");
            step.script_file = str_field(raw, "scriptFile");
            if step.run.is_none() && step.script_file.is_none() {
                bail!("script step \"{id}\" needs run or scriptFile");
            }
        }
    }

    Ok(step)
}

fn normalize_observe(
    raw: Option<&serde_yaml::Value>,
    step_id: &str,
) -> Result<Option<ObserveSpec>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    if !raw.is_mapping() {
        bail!("step \"{step_id}\" observe must be a mapping");
    }
    let label = str_field(raw, "label")
        .ok_or_else(|| anyhow!("step \"{step_id}\" observe missing label"))?;
    let r#as = if str_field(raw, "as").as_deref() == Some("artifact") {
        ObserveAs::Artifact
    } else {
        ObserveAs::Inline
    };

    let from = match raw.get("from") {
        None => ObserveFrom::Result,
        Some(v) if v.as_str() == Some("result") => ObserveFrom::Result,
        Some(v) => match v.get("file").and_then(|f| f.as_str()) {
            Some(file) => ObserveFrom::File(file.to_string()),
            None => bail!("step \"{step_id}\" observe.from must be \"result\" or {{ file }}"),
        },
    };

    Ok(Some(ObserveSpec { label, from, r#as }))
}

fn normalize_inputs(raw: Option<&serde_yaml::Value>) -> Result<Vec<WorkflowInput>> {
    let Some(seq) = raw.and_then(|r| r.as_sequence()) else {
        return Ok(Vec::new());
    };
    seq.iter()
        .enumerate()
        .map(|(i, r)| {
            if !r.is_mapping() {
                bail!("inputs[{i}] is not a mapping");
            }
            let name = str_field(r, "name").ok_or_else(|| anyhow!("inputs[{i}] missing name"))?;
            Ok(WorkflowInput {
                name,
                required: r.get("required").and_then(|v| v.as_bool()) == Some(true),
                // Allow non-string YAML scalars (e.g. `default: 3`) as defaults.
                default: r.get("default").map(|v| match v.as_str() {
                    Some(s) => s.to_string(),
                    None => serde_yaml::to_string(v)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                }),
                description: str_field(r, "description"),
            })
        })
        .collect()
}

/// Dependency inference: fold each step's data references to other steps'
/// results into its `depends_on` (union).
///   - agent: scan prompt / step-level guidance for `{{steps.<id>.result}}`
///     (unknown ids error immediately).
///   - script: scan inline `run` for `$WF_STEP_<SEG>_RESULT` (forward-matched
///     against known ids — `env_segment` is lossy, so no reverse mapping).
///
/// A reference means "must wait for it", so folding into depends_on is always
/// safe and never introduces a false dependency. `script_file` bodies are not
/// scanned; their deps must be declared explicitly. Self-references surface
/// as cycles in `assert_acyclic`.
fn infer_dependencies(steps: &mut [WorkflowStep], ids: &HashSet<String>) -> Result<()> {
    for s in steps.iter_mut() {
        let mut deps: HashSet<String> = s.depends_on.iter().cloned().collect();

        for id in extract_template_step_refs(s.prompt.as_deref())
            .into_iter()
            .chain(extract_template_step_refs(s.guidance.as_deref()))
        {
            if !ids.contains(&id) {
                bail!(
                    "step \"{}\" references unknown step \"{id}\" (in prompt/guidance)",
                    s.id
                );
            }
            deps.insert(id);
        }

        if s.kind == StepKind::Script {
            if let Some(run) = &s.run {
                for id in ids {
                    if id != &s.id && run.contains(&format!("WF_STEP_{}_RESULT", env_segment(id))) {
                        deps.insert(id.clone());
                    }
                }
            }
        }

        if !deps.is_empty() {
            let mut sorted: Vec<String> = deps.into_iter().collect();
            sorted.sort();
            s.depends_on = sorted;
        }
    }
    Ok(())
}

/// Cycle detection (DFS, three-color).
fn assert_acyclic(steps: &[WorkflowStep]) -> Result<()> {
    let dep_map: HashMap<&str, &[String]> = steps
        .iter()
        .map(|s| (s.id.as_str(), s.depends_on.as_slice()))
        .collect();
    // 0 = unvisited, 1 = on stack, 2 = done
    let mut state: HashMap<&str, u8> = HashMap::new();

    fn visit<'a>(
        id: &'a str,
        dep_map: &HashMap<&'a str, &'a [String]>,
        state: &mut HashMap<&'a str, u8>,
    ) -> Result<()> {
        match state.get(id).copied().unwrap_or(0) {
            2 => return Ok(()),
            1 => bail!("dependency cycle involving \"{id}\""),
            _ => {}
        }
        state.insert(id, 1);
        if let Some(deps) = dep_map.get(id) {
            for d in deps.iter() {
                visit(d.as_str(), dep_map, state)?;
            }
        }
        state.insert(id, 2);
        Ok(())
    }

    for s in steps {
        visit(&s.id, &dep_map, &mut state)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wf(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("{name}.md"));
        std::fs::write(&path, body).unwrap();
        path
    }

    const VALID: &str = r#"---
name: market-research
description: fan-out research
inputs:
  - { name: topic, required: true }
  - { name: depth, default: "standard" }
guidance: |
  keep it concise
steps:
  - id: research_tech
    kind: agent
    persona: researcher
    prompt: "research {{input.topic}}"
  - id: fetch
    kind: script
    run: |
      echo "$WF_INPUT_TOPIC" > out.txt
      echo done
  - id: summary
    kind: agent
    persona: analyst
    prompt: |
      tech: {{steps.research_tech.result}}
      raw: {{steps.fetch.result}}
    observe: { label: "report", from: result, as: inline }
---
body text
"#;

    #[test]
    fn parses_valid_workflow_and_infers_deps() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wf(dir.path(), "market-research", VALID);
        let def = parse_workflow_file(&path).unwrap();
        assert_eq!(def.name, "market-research");
        assert_eq!(def.inputs.len(), 2);
        assert!(def.inputs[0].required);
        assert_eq!(def.inputs[1].default.as_deref(), Some("standard"));
        assert_eq!(def.steps.len(), 3);
        // summary depends on both referenced steps (inferred, sorted)
        let summary = &def.steps[2];
        assert_eq!(summary.depends_on, vec!["fetch", "research_tech"]);
        let obs = summary.observe.as_ref().unwrap();
        assert_eq!(obs.label, "report");
        assert_eq!(obs.r#as, ObserveAs::Inline);
        assert_eq!(obs.from, ObserveFrom::Result);
    }

    #[test]
    fn script_env_ref_infers_dep() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"---
steps:
  - id: first
    kind: script
    run: echo hi
  - id: second
    kind: script
    run: echo "$WF_STEP_FIRST_RESULT"
---
"#;
        let path = write_wf(dir.path(), "chain", body);
        let def = parse_workflow_file(&path).unwrap();
        assert_eq!(def.steps[1].depends_on, vec!["first"]);
        // name falls back to file stem
        assert_eq!(def.name, "chain");
    }

    #[test]
    fn rejects_unknown_ref_dup_id_cycle_and_bad_kind() {
        let dir = tempfile::tempdir().unwrap();

        let bad_ref = r#"---
steps:
  - { id: a, kind: agent, persona: p, prompt: "{{steps.zzz.result}}" }
---
"#;
        let e = parse_workflow_file(&write_wf(dir.path(), "r1", bad_ref)).unwrap_err();
        assert!(e.to_string().contains("unknown step"), "{e}");

        let dup = r#"---
steps:
  - { id: a, kind: script, run: echo 1 }
  - { id: a, kind: script, run: echo 2 }
---
"#;
        let e = parse_workflow_file(&write_wf(dir.path(), "r2", dup)).unwrap_err();
        assert!(e.to_string().contains("duplicate step id"), "{e}");

        let cycle = r#"---
steps:
  - { id: a, kind: script, run: echo 1, dependsOn: [b] }
  - { id: b, kind: script, run: echo 2, dependsOn: [a] }
---
"#;
        let e = parse_workflow_file(&write_wf(dir.path(), "r3", cycle)).unwrap_err();
        assert!(e.to_string().contains("cycle"), "{e}");

        let bad_kind = r#"---
steps:
  - { id: a, kind: nope, run: echo 1 }
---
"#;
        let e = parse_workflow_file(&write_wf(dir.path(), "r4", bad_kind)).unwrap_err();
        assert!(e.to_string().contains("agent|script"), "{e}");
    }

    #[test]
    fn rejects_agent_without_persona_and_script_without_cmd() {
        let dir = tempfile::tempdir().unwrap();
        let no_persona = "---\nsteps:\n  - { id: a, kind: agent, prompt: hi }\n---\n";
        let e = parse_workflow_file(&write_wf(dir.path(), "p1", no_persona)).unwrap_err();
        assert!(e.to_string().contains("missing persona"), "{e}");

        let no_cmd = "---\nsteps:\n  - { id: a, kind: script }\n---\n";
        let e = parse_workflow_file(&write_wf(dir.path(), "p2", no_cmd)).unwrap_err();
        assert!(e.to_string().contains("run or scriptFile"), "{e}");
    }

    #[test]
    fn rejects_step_ref_in_workflow_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"---
guidance: "use {{steps.a.result}}"
steps:
  - { id: a, kind: script, run: echo 1 }
---
"#;
        let e = parse_workflow_file(&write_wf(dir.path(), "g1", body)).unwrap_err();
        assert!(e.to_string().contains("workflow-level guidance"), "{e}");
    }

    #[test]
    fn registry_skips_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        write_wf(dir.path(), "good", VALID);
        write_wf(dir.path(), "bad", "no frontmatter here");
        let reg = WorkflowRegistry::new(dir.path().to_path_buf());
        assert_eq!(reg.list().len(), 1);
        assert!(reg.get("market-research").is_some());
        assert!(reg.get("bad").is_none());
    }

    #[test]
    fn no_frontmatter_errors() {
        let dir = tempfile::tempdir().unwrap();
        let e = parse_workflow_file(&write_wf(dir.path(), "nf", "just text")).unwrap_err();
        assert!(e.to_string().contains("no frontmatter"));
    }
}

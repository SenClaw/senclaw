//! Template rendering + script env construction.
//!
//! Port of `SemaClaw/src/workflow/template.ts`.
//!
//! Two interpolation channels, deliberately separate:
//!   - agent prompt / guidance: `{{input.X}}` / `{{steps.ID.result}}` text
//!     substitution (safe — the value goes into LLM input).
//!   - script: values are NOT interpolated into the shell command (injection
//!     risk); they are exported as env vars the script reads itself.
//!
//! The template language is intentionally logic-free: variable substitution
//! only, no conditionals/loops.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use super::types::RenderContext;

/// Matches `{{ ... }}`, capturing the trimmed inner expression.
static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*([^}]+?)\s*\}\}").unwrap());

/// Matches `{{steps.<id>.result}}` specifically (same id charset as
/// `resolve_expr`); used for dependency inference.
static STEP_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*steps\.([A-Za-z0-9_-]+)\.result\s*\}\}").unwrap());

static INPUT_EXPR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^input\.([A-Za-z0-9_]+)$").unwrap());
static STEP_EXPR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^steps\.([A-Za-z0-9_-]+)\.result$").unwrap());

/// Extract every `{{steps.<id>.result}}` step id referenced in `text`
/// (original order, duplicates preserved). Used by the registry to fold data
/// references into `depends_on` — referencing a step's result implies
/// depending on it.
pub fn extract_template_step_refs(text: Option<&str>) -> Vec<String> {
    let Some(text) = text else {
        return Vec::new();
    };
    STEP_REF
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect()
}

/// Render template text: substitute `{{input.X}}` and `{{steps.ID.result}}`.
/// Unknown variables become the empty string (missing upstream results can't
/// happen — the DAG scheduler guarantees ordering; missing inputs mean the
/// user didn't pass them).
pub fn render(text: Option<&str>, ctx: &RenderContext) -> String {
    let Some(text) = text else {
        return String::new();
    };
    PLACEHOLDER
        .replace_all(text, |caps: &regex::Captures<'_>| {
            resolve_expr(&caps[1], ctx).unwrap_or_default()
        })
        .into_owned()
}

/// Resolve a single expression; `None` = unknown.
fn resolve_expr(expr: &str, ctx: &RenderContext) -> Option<String> {
    if let Some(m) = INPUT_EXPR.captures(expr) {
        return ctx.inputs.get(&m[1]).cloned();
    }
    if let Some(m) = STEP_EXPR.captures(expr) {
        return ctx.step_results.get(&m[1]).cloned();
    }
    None
}

/// Normalize a name into a valid env-var segment (uppercase + non-alnum → `_`).
pub fn env_segment(name: &str) -> String {
    name.to_uppercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Build env vars for a script step:
///   `WF_INPUT_<NAME>`      — each run input
///   `WF_STEP_<ID>_RESULT`  — each completed step's result
///   `WF_RUN_DIR`           — the run's shared workspace
///   `WF_OBSERVE_DIR`       — observe convention dir (`<run_dir>/.observe`)
///   `WF_WORKFLOW_DIR`      — the workflow's persistent dir (optional)
///
/// Does NOT include the process env — the caller (step_runners) merges it.
/// Kept pure for unit testing.
pub fn build_script_env(
    ctx: &RenderContext,
    observe_dir: &str,
    workflow_dir: Option<&str>,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("WF_RUN_DIR".to_string(), ctx.run_dir.clone());
    env.insert("WF_OBSERVE_DIR".to_string(), observe_dir.to_string());
    if let Some(dir) = workflow_dir {
        env.insert("WF_WORKFLOW_DIR".to_string(), dir.to_string());
    }
    for (k, v) in &ctx.inputs {
        env.insert(format!("WF_INPUT_{}", env_segment(k)), v.clone());
    }
    for (id, result) in &ctx.step_results {
        env.insert(
            format!("WF_STEP_{}_RESULT", env_segment(id)),
            result.clone(),
        );
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RenderContext {
        RenderContext {
            inputs: HashMap::from([("topic".to_string(), "rust".to_string())]),
            step_results: HashMap::from([("fetch".to_string(), "42 lines".to_string())]),
            run_dir: "/tmp/run".to_string(),
        }
    }

    #[test]
    fn render_substitutes_inputs_and_steps() {
        let out = render(
            Some("Topic: {{input.topic}}, fetched: {{ steps.fetch.result }}"),
            &ctx(),
        );
        assert_eq!(out, "Topic: rust, fetched: 42 lines");
    }

    #[test]
    fn render_unknown_becomes_empty() {
        assert_eq!(render(Some("x{{input.nope}}y{{bogus}}z"), &ctx()), "xyz");
        assert_eq!(render(None, &ctx()), "");
    }

    #[test]
    fn extract_step_refs_finds_ids() {
        let refs = extract_template_step_refs(Some(
            "a {{steps.one.result}} b {{ steps.two-x.result }} c {{input.topic}}",
        ));
        assert_eq!(refs, vec!["one", "two-x"]);
        assert!(extract_template_step_refs(None).is_empty());
    }

    #[test]
    fn env_segment_uppercases_and_replaces() {
        assert_eq!(env_segment("fetch-data.v2"), "FETCH_DATA_V2");
    }

    #[test]
    fn script_env_contains_wf_vars() {
        let env = build_script_env(&ctx(), "/tmp/run/.observe", Some("/tmp/run"));
        assert_eq!(env["WF_RUN_DIR"], "/tmp/run");
        assert_eq!(env["WF_OBSERVE_DIR"], "/tmp/run/.observe");
        assert_eq!(env["WF_WORKFLOW_DIR"], "/tmp/run");
        assert_eq!(env["WF_INPUT_TOPIC"], "rust");
        assert_eq!(env["WF_STEP_FETCH_RESULT"], "42 lines");
    }
}

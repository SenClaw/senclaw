//! Install / remove skills bundled with a Space App.
//!
//! A manifest may declare a `skills` array, each `{ name, path }` pointing at a
//! skill folder (containing `SKILL.md`) inside the app. On install we copy each
//! into the managed skills dir as `<app_id>__<skill>` and drop a
//! `.senclaw-app.json` marker so the scanner labels it `app:<app_id>` — which
//! makes it read-only in the UI/API. On app uninstall we remove every skill dir
//! carrying that app's marker.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::Config;

const MARKER: &str = ".senclaw-app.json";

fn managed_dir(config: &Config) -> &Path {
    &config.paths.managed_skills_dir
}

fn app_skill_dest(config: &Config, _app_id: &str, skill_name: &str) -> PathBuf {
    // The scanner uses the directory name as the skill's display name, so keep
    // it clean (just the skill name). Ownership is tracked via the marker file,
    // not the directory name.
    managed_dir(config).join(skill_name)
}

fn has_skill_md(dir: &Path) -> bool {
    ["SKILL.md", "skill.md", "Skill.md"]
        .iter()
        .any(|n| dir.join(n).is_file())
}

/// Install (or refresh) all skills declared in the manifest. Idempotent:
/// re-installing overwrites the existing copy.
pub fn install_app_skills(config: &Config, app_id: &str, app_dir: &Path, manifest: &Value) {
    let Some(skills) = manifest.get("skills").and_then(Value::as_array) else {
        return;
    };
    let _ = fs::create_dir_all(managed_dir(config));

    for sk in skills {
        let Some(name) = sk.get("name").and_then(Value::as_str) else {
            continue;
        };
        let rel = sk.get("path").and_then(Value::as_str).unwrap_or(name);
        let src = app_dir.join(rel);
        if !has_skill_md(&src) {
            tracing::warn!("[space-skills] app '{app_id}' skill '{name}': no SKILL.md at {src:?}");
            continue;
        }
        let dest = app_skill_dest(config, app_id, name);
        let _ = fs::remove_dir_all(&dest);
        if let Err(e) = copy_dir_all(&src, &dest) {
            tracing::warn!("[space-skills] copy '{name}' for '{app_id}' failed: {e}");
            continue;
        }
        let marker = serde_json::json!({ "app_id": app_id, "skill": name });
        let _ = fs::write(dest.join(MARKER), marker.to_string());
        // Space App manifests carry each skill's trigger phrases in the manifest
        // (`skills[].triggers`), but the skill scanner only reads triggers from the
        // SKILL.md frontmatter (`src/skills/metadata.rs`). Merge them in so the
        // installed app skill actually auto-loads on a keyword match — for both the
        // main agent and dispatched sub-agents (`zen_core::engine::match_skill_name`).
        let triggers: Vec<String> = sk
            .get("triggers")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        merge_triggers_into_skill_md(&dest, &triggers);
        tracing::info!("[space-skills] installed skill '{name}' for app '{app_id}'");
    }
}

/// Remove every managed skill dir tagged with this app's marker.
pub fn remove_app_skills(config: &Config, app_id: &str) {
    let Ok(items) = fs::read_dir(managed_dir(config)) else {
        return;
    };
    for item in items.flatten() {
        let dir = item.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(dir.join(MARKER)) else {
            continue;
        };
        let owned = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| v.get("app_id").and_then(|x| x.as_str()).map(str::to_string));
        if owned.as_deref() == Some(app_id) {
            let _ = fs::remove_dir_all(&dir);
            tracing::info!("[space-skills] removed skill dir {dir:?} for app '{app_id}'");
        }
    }
}

/// YAML-quote a scalar for a double-quoted flow scalar (mirrors the writer in
/// `ui_server::skills::build_skill_md` so the frontmatter parses identically).
fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Merge `triggers` into the installed skill's `SKILL.md` frontmatter.
/// Locates the skill markdown under `dest`, then delegates the string edit to
/// [`inject_triggers_frontmatter`]. No-op when there are no triggers, the file
/// has no frontmatter, or the author already declared their own `triggers:`.
fn merge_triggers_into_skill_md(dest: &Path, triggers: &[String]) {
    let cleaned: Vec<String> = triggers
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if cleaned.is_empty() {
        return;
    }
    let Some(md_path) = ["SKILL.md", "skill.md", "Skill.md"]
        .iter()
        .map(|n| dest.join(n))
        .find(|p| p.is_file())
    else {
        return;
    };
    let Ok(raw) = fs::read_to_string(&md_path) else {
        return;
    };
    match inject_triggers_frontmatter(&raw, &cleaned) {
        Some(updated) => {
            if let Err(e) = fs::write(&md_path, updated) {
                tracing::warn!("[space-skills] write triggers to {md_path:?} failed: {e}");
            } else {
                tracing::info!(
                    "[space-skills] merged {} trigger(s) into {md_path:?}",
                    cleaned.len()
                );
            }
        }
        None => {
            tracing::debug!("[space-skills] {md_path:?}: triggers not merged (absent/own/no-fm)");
        }
    }
}

/// Pure frontmatter edit: return an updated SKILL.md string with a `triggers:`
/// block inserted at the end of the YAML frontmatter, or `None` when nothing
/// should change. Returns `None` if `raw` has no leading `---` frontmatter, the
/// frontmatter is unterminated, or a `triggers:`/`trigger:` key is already
/// present (author intent wins). Line endings are normalized to `\n`.
fn inject_triggers_frontmatter(raw: &str, triggers: &[String]) -> Option<String> {
    if triggers.is_empty() {
        return None;
    }
    let mut lines = raw.lines();
    if lines.next().map(str::trim) != Some("---") {
        return None; // no frontmatter to extend
    }
    let mut frontmatter: Vec<&str> = Vec::new();
    let mut body: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines {
        if !closed && line.trim() == "---" {
            closed = true;
            continue;
        }
        if closed {
            body.push(line);
        } else {
            frontmatter.push(line);
        }
    }
    if !closed {
        return None; // unterminated frontmatter
    }
    // Respect an author's own triggers.
    if frontmatter.iter().any(|l| {
        let t = l.trim_start();
        t.starts_with("triggers:") || t.starts_with("trigger:")
    }) {
        return None;
    }

    let mut out = String::from("---\n");
    for l in &frontmatter {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str("triggers:\n");
    for t in triggers {
        out.push_str("  - ");
        out.push_str(&yaml_quote(t));
        out.push('\n');
    }
    out.push_str("---\n");
    for l in &body {
        out.push_str(l);
        out.push('\n');
    }
    Some(out)
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ty.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::inject_triggers_frontmatter;

    fn trigs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn inserts_triggers_before_closing_delimiter() {
        let raw = "---\nname: crm-lookup\ndescription: Look up a customer\n---\n\n# Body\ntext\n";
        let out =
            inject_triggers_frontmatter(&raw, &trigs(&["tìm khách hàng", "look up customer"]))
                .expect("should inject");
        assert_eq!(
            out,
            "---\nname: crm-lookup\ndescription: Look up a customer\ntriggers:\n  - \"tìm khách hàng\"\n  - \"look up customer\"\n---\n\n# Body\ntext\n"
        );
    }

    #[test]
    fn respects_existing_triggers() {
        let raw = "---\nname: x\ntriggers:\n  - existing\n---\nbody\n";
        assert!(inject_triggers_frontmatter(&raw, &trigs(&["new"])).is_none());
    }

    #[test]
    fn skips_when_no_frontmatter() {
        let raw = "# Just a body\nno frontmatter here\n";
        assert!(inject_triggers_frontmatter(&raw, &trigs(&["t"])).is_none());
    }

    #[test]
    fn skips_when_unterminated_frontmatter() {
        let raw = "---\nname: x\ndescription: y\n";
        assert!(inject_triggers_frontmatter(&raw, &trigs(&["t"])).is_none());
    }

    #[test]
    fn empty_triggers_is_noop() {
        let raw = "---\nname: x\n---\nbody\n";
        assert!(inject_triggers_frontmatter(&raw, &[]).is_none());
    }

    #[test]
    fn preserves_horizontal_rule_in_body() {
        let raw = "---\nname: x\n---\nintro\n---\nafter rule\n";
        let out = inject_triggers_frontmatter(&raw, &trigs(&["go"])).expect("inject");
        // Only the first `---` after the open closes the frontmatter; the body
        // `---` horizontal rule must survive.
        assert!(out.contains("triggers:\n  - \"go\"\n---\nintro\n---\nafter rule\n"));
    }

    #[test]
    fn quotes_special_characters() {
        let raw = "---\nname: x\n---\nbody\n";
        let out = inject_triggers_frontmatter(&raw, &trigs(&["say \"hi\""])).expect("inject");
        assert!(out.contains("  - \"say \\\"hi\\\"\"\n"));
    }
}

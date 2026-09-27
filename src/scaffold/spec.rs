//! `template.json` — the one file that makes a directory a template.
//!
//! Everything in it is optional except `name`. A template that declares nothing
//! but its name still works: its kind is inferred from what it contains
//! (`senclaw-manifest.json` → an app, `SKILL.md` → a skill), and it is rendered
//! against the built-in variables alone. The file exists so a template *can*
//! ask for more — an extra variable, a different payload root, a line of
//! next-steps advice — not so every template must.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What the template produces. Decides where the output goes by default and
/// which post-render validation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A Space App: a directory with `senclaw-manifest.json` at its root.
    App,
    /// A skill: a directory with `SKILL.md` at its root.
    Skill,
    /// A virtual-agent persona: a single `<name>.md` with YAML frontmatter.
    SubAgent,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::App => "app",
            Kind::Skill => "skill",
            Kind::SubAgent => "sub-agent",
        }
    }

    /// Accepts the spellings a person types on a command line, and the ones a
    /// template author writes in JSON.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "app" | "space-app" | "spaceapp" => Some(Kind::App),
            "skill" => Some(Kind::Skill),
            "sub-agent" | "subagent" | "agent" | "persona" => Some(Kind::SubAgent),
            _ => None,
        }
    }
}

/// The language a template's app is written in. Only used to pick a default
/// template (`--lang go` → `app-go`) and to print the right next steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Rust,
    Go,
    Node,
    Python,
}

impl Lang {
    pub const ALL: [Lang; 4] = [Lang::Rust, Lang::Go, Lang::Node, Lang::Python];

    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Go => "go",
            Lang::Node => "node",
            Lang::Python => "python",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Some(Lang::Rust),
            "go" | "golang" => Some(Lang::Go),
            "node" | "nodejs" | "js" | "javascript" | "ts" | "typescript" => Some(Lang::Node),
            "python" | "py" | "python3" => Some(Lang::Python),
            _ => None,
        }
    }

    /// The template name this language maps to when the user says `--lang`
    /// instead of naming a template.
    pub fn template_name(self) -> String {
        format!("app-{}", self.as_str())
    }
}

/// A variable the template asks for beyond the built-in set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VarSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// May itself contain `{{…}}`, resolved against the variables defined
    /// before it — which is how `"default": "{{id}}-mcp"` works.
    #[serde(default)]
    pub default: Option<String>,
    /// A variable with no default and no `--var` is an error rather than an
    /// empty string, so a half-filled scaffold never reaches disk.
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateSpec {
    pub name: String,
    #[serde(default)]
    pub kind: Option<Kind>,
    #[serde(default)]
    pub lang: Option<Lang>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub variables: Vec<VarSpec>,
    /// Subdirectory holding the payload. Defaults to `files` when that
    /// directory exists, otherwise the template directory itself.
    #[serde(default)]
    pub root: Option<String>,
    /// Glob-free path prefixes and names never copied. Merged with the
    /// always-ignored set (`.git`, `template.json`, `node_modules`, `target`).
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Printed after a successful create. The commands to run next — printed,
    /// never executed: a scaffolder that runs `cargo build` for you is a
    /// scaffolder that hangs for four minutes with no output.
    #[serde(default)]
    pub post_create: Vec<String>,
    /// Refuse to render with an older CLI, so a template that uses a variable
    /// added later fails with a sentence instead of leaving `{{…}}` in the file.
    #[serde(default)]
    pub min_cli_version: Option<String>,
}

impl TemplateSpec {
    /// The spec for a template directory that has no `template.json`.
    pub fn inferred(name: &str, dir: &Path) -> TemplateSpec {
        TemplateSpec {
            name: name.to_string(),
            kind: infer_kind(dir),
            lang: None,
            description: None,
            variables: Vec::new(),
            root: None,
            ignore: Vec::new(),
            post_create: Vec::new(),
            min_cli_version: None,
        }
    }

    pub fn parse(raw: &str, name: &str) -> Result<TemplateSpec> {
        let mut spec: TemplateSpec =
            serde_json::from_str(raw).context("template.json không phải JSON hợp lệ")?;
        if spec.name.trim().is_empty() {
            spec.name = name.to_string();
        }
        Ok(spec)
    }

    /// Load `<dir>/template.json`, falling back to inference when absent.
    pub fn load(dir: &Path, name: &str) -> Result<TemplateSpec> {
        let path = dir.join("template.json");
        if !path.exists() {
            return Ok(TemplateSpec::inferred(name, dir));
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("không đọc được {}", path.display()))?;
        let mut spec = TemplateSpec::parse(&raw, name)
            .with_context(|| format!("{} không hợp lệ", path.display()))?;
        if spec.kind.is_none() {
            spec.kind = infer_kind(&self::payload_root(dir, &spec));
        }
        Ok(spec)
    }

    pub fn kind_or_default(&self) -> Kind {
        self.kind.unwrap_or(Kind::App)
    }
}

/// Where the files to copy actually live.
pub fn payload_root(dir: &Path, spec: &TemplateSpec) -> std::path::PathBuf {
    match spec.root.as_deref() {
        Some(r) if !r.trim().is_empty() => dir.join(r.trim()),
        _ => {
            let files = dir.join("files");
            if files.is_dir() {
                files
            } else {
                dir.to_path_buf()
            }
        }
    }
}

/// Guess the kind from the payload, so a template author who forgets the field
/// still gets the right validation.
fn infer_kind(dir: &Path) -> Option<Kind> {
    if dir.join("senclaw-manifest.json").exists() {
        return Some(Kind::App);
    }
    if dir.join("SKILL.md").exists() {
        return Some(Kind::Skill);
    }
    None
}

/// Never copied, whatever the template says: build output and VCS metadata are
/// never part of a scaffold, and `template.json` describes the template rather
/// than belonging to the thing being created.
pub const ALWAYS_IGNORED: &[&str] = &[
    ".git",
    ".DS_Store",
    "template.json",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "dist",
    ".senclaw",
];

/// True when a path relative to the payload root must not be copied.
pub fn is_ignored(rel: &str, extra: &[String]) -> bool {
    rel.split('/').any(|seg| {
        ALWAYS_IGNORED.contains(&seg) || extra.iter().any(|e| e.trim() == seg)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_accepts_what_people_type() {
        assert_eq!(Kind::parse("app"), Some(Kind::App));
        assert_eq!(Kind::parse("sub-agent"), Some(Kind::SubAgent));
        assert_eq!(Kind::parse("subagent"), Some(Kind::SubAgent));
        assert_eq!(Kind::parse("sub_agent"), Some(Kind::SubAgent));
        assert_eq!(Kind::parse("persona"), Some(Kind::SubAgent));
        assert_eq!(Kind::parse("Skill"), Some(Kind::Skill));
        assert_eq!(Kind::parse("widget"), None);
    }

    #[test]
    fn lang_maps_to_a_template_name() {
        assert_eq!(Lang::parse("golang").unwrap().template_name(), "app-go");
        assert_eq!(Lang::parse("ts").unwrap().template_name(), "app-node");
        assert_eq!(Lang::parse("rs").unwrap().template_name(), "app-rust");
        assert_eq!(Lang::parse("py").unwrap().template_name(), "app-python");
        assert!(Lang::parse("ruby").is_none());
    }

    #[test]
    fn minimal_template_json_parses() {
        let spec = TemplateSpec::parse(r#"{"name":"app-rust"}"#, "x").unwrap();
        assert_eq!(spec.name, "app-rust");
        assert_eq!(spec.kind_or_default(), Kind::App);
        assert!(spec.variables.is_empty());
    }

    #[test]
    fn full_template_json_parses() {
        let spec = TemplateSpec::parse(
            r#"{
              "name": "app-go", "kind": "app", "lang": "go",
              "description": "d",
              "variables": [{"name":"mcp_name","default":"{{id}}-mcp"}],
              "ignore": ["fixtures"],
              "postCreate": ["go build ."],
              "minCliVersion": "0.6.0"
            }"#,
            "x",
        )
        .unwrap();
        assert_eq!(spec.lang, Some(Lang::Go));
        assert_eq!(spec.variables[0].default.as_deref(), Some("{{id}}-mcp"));
        assert_eq!(spec.post_create, ["go build ."]);
        assert_eq!(spec.min_cli_version.as_deref(), Some("0.6.0"));
    }

    #[test]
    fn build_output_is_never_copied() {
        assert!(is_ignored("node_modules/x/y.js", &[]));
        assert!(is_ignored("target/debug/app", &[]));
        assert!(is_ignored("template.json", &[]));
        assert!(is_ignored("web/dist/index.html", &[]));
        assert!(is_ignored("fixtures/a", &["fixtures".to_string()]));
        assert!(!is_ignored("src/main.rs", &[]));
        // Not a prefix match: a file *named* like a directory we skip is only
        // skipped when the whole segment matches.
        assert!(!is_ignored("src/targets.rs", &[]));
    }
}

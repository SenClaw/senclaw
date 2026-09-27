//! Skill frontmatter parsing and metadata schema.
//!
//! Parses the YAML frontmatter of a `SKILL.md` into [`SkillMetadata`] using
//! `serde_yaml`, so we support both nested YAML and OpenClaw's single-line
//! `metadata.openclaw: '{...json...}'` form.
//!
//! ## OpenClaw compatibility
//!
//! In addition to the Anthropic/Claude fields (`name`, `description`,
//! `allowed-tools`, `when-to-use`, `model`, `max-thinking-tokens`,
//! `disable-model-invocation`, `argument-hint`, `version`) we accept the
//! OpenClaw fields:
//!
//! - `triggers` — keyword list that hints when the skill applies. Either a
//!   YAML sequence (`[a, b]`) or a comma/space-separated string.
//! - `user-invocable` — bool (default `true`): exposed as a slash command.
//! - `command-dispatch` / `command-tool` — route a slash command straight to
//!   a tool.
//! - `params` — custom argument schema (`name`, `type`, `required`,
//!   `description`). OpenClaw does not standardise this, so it is our own
//!   convention; it is surfaced to the model on activation.
//! - `metadata.openclaw` (aliases: `metadata.clawdbot`, `metadata.clawdis`)
//!   with `os`, `requires.{env,bins,anyBins,config}`, and `primaryEnv` for
//!   load-time gating.
//!
//! Env *injection* (`skills.entries.<name>.env` in `config.json`) lives in
//! [`crate::skills::config`], not here — frontmatter only declares what a
//! skill *requires*, config provides the values.

use serde_yaml::Value as Yaml;

/// How a skill is activated, declared by the frontmatter `use` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkillUseMode {
    /// Default. The skill loads only when triggered — matched by
    /// triggers/when-to-use (pre-trigger-skill), invoked via the `Skill` tool,
    /// or called explicitly with `#name` / `/name`.
    #[default]
    Trigger,
    /// The skill's full instructions are injected into every prompt (always-on
    /// behavior, e.g. a persistent persona or house-style guide). Its env and
    /// referenced tools are activated up front, like a force-loaded skill.
    Always,
}

impl SkillUseMode {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" | "all" | "persistent" => SkillUseMode::Always,
            _ => SkillUseMode::Trigger,
        }
    }
}

/// A custom argument the skill declares (OpenClaw-style `params`).
#[derive(Debug, Clone, PartialEq)]
pub struct SkillParam {
    pub name: String,
    /// JSON-schema-ish type name (`string`, `number`, `boolean`, …).
    pub type_: String,
    pub required: bool,
    pub description: Option<String>,
}

/// Parsed skill metadata from YAML frontmatter.
#[derive(Debug, Clone)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub allowed_tools: Vec<String>,
    pub when_to_use: Option<String>,
    pub model: Option<String>,
    pub max_thinking_tokens: Option<u32>,
    pub disable_model_invocation: bool,
    pub argument_hint: Option<String>,
    pub version: Option<String>,
    /// Activation mode (`use: always|trigger`). Default [`SkillUseMode::Trigger`].
    pub use_mode: SkillUseMode,

    // --- OpenClaw-compatible additions ---
    /// Keyword triggers that hint when this skill applies.
    pub triggers: Vec<String>,
    /// Whether the skill is exposed as a user slash command (default true).
    pub user_invocable: bool,
    /// When `Some("tool")`, a slash command dispatches straight to a tool.
    pub command_dispatch: Option<String>,
    /// Tool name invoked when `command_dispatch == Some("tool")`.
    pub command_tool: Option<String>,
    /// OS allow-list (`darwin`/`linux`/`win32`, or `macos`/`windows`).
    pub os: Vec<String>,
    /// Env vars that must all be present for the skill to load.
    pub requires_env: Vec<String>,
    /// Binaries that must all be on PATH.
    pub requires_bins: Vec<String>,
    /// Binaries where at least one must be on PATH.
    pub requires_any_bins: Vec<String>,
    /// Config keys the skill reads (declared only; not gated here).
    pub requires_config: Vec<String>,
    /// The skill's primary credential env var (links to config `apiKey`).
    pub primary_env: Option<String>,
    /// Custom argument schema (our convention).
    pub params: Vec<SkillParam>,
}

impl Default for SkillMetadata {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            allowed_tools: Vec::new(),
            when_to_use: None,
            model: None,
            max_thinking_tokens: None,
            disable_model_invocation: false,
            argument_hint: None,
            version: None,
            use_mode: SkillUseMode::Trigger,
            triggers: Vec::new(),
            user_invocable: true,
            command_dispatch: None,
            command_tool: None,
            os: Vec::new(),
            requires_env: Vec::new(),
            requires_bins: Vec::new(),
            requires_any_bins: Vec::new(),
            requires_config: Vec::new(),
            primary_env: None,
            params: Vec::new(),
        }
    }
}

impl SkillMetadata {
    /// Returns `Some(reason)` if the skill cannot run on this host (failing an
    /// `os` / `requires` gate). `None` means the skill is eligible.
    ///
    /// `requires.config` is intentionally not gated — we have no global
    /// `openclaw.json` to resolve it against, so it is declarative only.
    pub fn ineligible_reason(&self) -> Option<String> {
        if !self.os.is_empty() {
            let cur = std::env::consts::OS;
            if !self.os.iter().any(|o| os_matches(o, cur)) {
                return Some(format!(
                    "os not supported (requires {:?}, host is {cur})",
                    self.os
                ));
            }
        }
        for e in &self.requires_env {
            if std::env::var(e).map(|v| v.is_empty()).unwrap_or(true) {
                return Some(format!("missing required env var: {e}"));
            }
        }
        for b in &self.requires_bins {
            if !bin_exists(b) {
                return Some(format!("missing required binary: {b}"));
            }
        }
        if !self.requires_any_bins.is_empty()
            && !self.requires_any_bins.iter().any(|b| bin_exists(b))
        {
            return Some(format!(
                "none of the required binaries are installed: {:?}",
                self.requires_any_bins
            ));
        }
        None
    }

    /// Convenience: whether the skill passes its load-time gates.
    pub fn is_eligible(&self) -> bool {
        self.ineligible_reason().is_none()
    }
}

/// Parse YAML frontmatter into [`SkillMetadata`].
///
/// `default_name` / `default_desc` fill in when the frontmatter omits those
/// keys (e.g. the directory name).
pub fn parse_skill_metadata(
    content: &str,
    default_name: &str,
    default_desc: &str,
) -> SkillMetadata {
    let mut meta = SkillMetadata {
        name: default_name.to_string(),
        description: default_desc.to_string(),
        ..Default::default()
    };

    let root =
        match extract_frontmatter(content).and_then(|fm| serde_yaml::from_str::<Yaml>(fm).ok()) {
            Some(v) if v.is_mapping() => v,
            _ => return meta,
        };

    if let Some(s) = root.get("name").and_then(yaml_str) {
        meta.name = s;
    }
    if let Some(s) = root.get("description").and_then(yaml_str) {
        meta.description = s;
    }
    if let Some(v) = get_any(&root, &["allowed-tools", "allowed_tools", "allow_tools"]) {
        meta.allowed_tools = as_string_list(v);
    }
    meta.when_to_use = get_any(&root, &["when-to-use", "when_to_use"]).and_then(yaml_str);
    meta.model = root.get("model").and_then(yaml_str);
    meta.max_thinking_tokens =
        get_any(&root, &["max-thinking-tokens", "max_thinking_tokens"]).and_then(yaml_u32);
    if let Some(b) = get_any(
        &root,
        &["disable-model-invocation", "disable_model_invocation"],
    )
    .and_then(yaml_bool)
    {
        meta.disable_model_invocation = b;
    }
    meta.argument_hint = get_any(&root, &["argument-hint", "argument_hint"]).and_then(yaml_str);
    meta.version = root.get("version").and_then(yaml_str);
    if let Some(s) = get_any(&root, &["use", "use-mode", "use_mode"]).and_then(yaml_str) {
        meta.use_mode = SkillUseMode::parse(&s);
    }

    // --- OpenClaw additions ---
    if let Some(v) = get_any(&root, &["triggers", "trigger"]) {
        meta.triggers = as_string_list(v);
    }
    if let Some(b) = get_any(&root, &["user-invocable", "user_invocable"]).and_then(yaml_bool) {
        meta.user_invocable = b;
    }
    meta.command_dispatch =
        get_any(&root, &["command-dispatch", "command_dispatch"]).and_then(yaml_str);
    meta.command_tool = get_any(&root, &["command-tool", "command_tool"]).and_then(yaml_str);
    meta.params = parse_params(get_any(&root, &["params", "parameters"]));

    // metadata.openclaw gating block.
    if let Some(oc) = extract_openclaw(&root) {
        if let Some(v) = oc.get("os") {
            meta.os = as_string_list(v);
        }
        meta.primary_env = get_any(&oc, &["primaryEnv", "primary_env"]).and_then(yaml_str);
        if let Some(req) = oc.get("requires") {
            meta.requires_env = req.get("env").map(as_string_list).unwrap_or_default();
            meta.requires_bins = req.get("bins").map(as_string_list).unwrap_or_default();
            meta.requires_any_bins = get_any(req, &["anyBins", "any_bins"])
                .map(as_string_list)
                .unwrap_or_default();
            meta.requires_config = req.get("config").map(as_string_list).unwrap_or_default();
        }
    }

    meta
}

/// Return the frontmatter slice (between the leading and next `---`), if any.
pub fn extract_frontmatter(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// Return the markdown body (everything after the frontmatter), trimmed.
pub fn extract_body(content: &str) -> String {
    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            return rest[end + 4..].trim().to_string();
        }
    }
    content.trim().to_string()
}

// --- helpers ---

fn get_any<'a>(v: &'a Yaml, keys: &[&str]) -> Option<&'a Yaml> {
    keys.iter().find_map(|k| v.get(*k))
}

fn yaml_str(v: &Yaml) -> Option<String> {
    v.as_str()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn yaml_u32(v: &Yaml) -> Option<u32> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn yaml_bool(v: &Yaml) -> Option<bool> {
    v.as_bool()
        .or_else(|| match v.as_str().map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "true" => Some(true),
            Some(s) if s == "false" => Some(false),
            _ => None,
        })
}

/// Coerce a YAML value into a list of strings. Accepts a sequence, or a
/// single string split on commas / whitespace.
fn as_string_list(v: &Yaml) -> Vec<String> {
    match v {
        Yaml::Sequence(seq) => seq
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        Yaml::String(s) => s
            .split(|c: char| c == ',' || c.is_whitespace())
            .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\''))
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_params(v: Option<&Yaml>) -> Vec<SkillParam> {
    let seq = match v.and_then(|x| x.as_sequence()) {
        Some(s) => s,
        None => return Vec::new(),
    };
    seq.iter()
        .filter_map(|item| {
            let name = item.get("name").and_then(yaml_str)?;
            Some(SkillParam {
                name,
                type_: item
                    .get("type")
                    .and_then(yaml_str)
                    .unwrap_or_else(|| "string".into()),
                required: item.get("required").and_then(yaml_bool).unwrap_or(false),
                description: item.get("description").and_then(yaml_str),
            })
        })
        .collect()
}

/// Locate the OpenClaw metadata object, handling all three shapes:
/// 1. a literal top-level key `metadata.openclaw` (single-line JSON string),
/// 2. `metadata` → `openclaw` nested mapping,
/// 3. `metadata` whose value is a single-line JSON string.
///
/// Aliases `clawdbot` / `clawdis` are accepted alongside `openclaw`.
fn extract_openclaw(root: &Yaml) -> Option<Yaml> {
    const NS: [&str; 3] = ["openclaw", "clawdbot", "clawdis"];

    for ns in NS {
        let dotted = format!("metadata.{ns}");
        if let Some(v) = root.get(dotted.as_str()) {
            return Some(coerce_obj(v));
        }
    }

    let metadata = coerce_obj(root.get("metadata")?);
    for ns in NS {
        if let Some(v) = metadata.get(ns) {
            return Some(coerce_obj(v));
        }
    }
    None
}

/// If `v` is a string, try to parse it as YAML/JSON (OpenClaw stores
/// `metadata` as a single-line JSON object). Otherwise return it as-is.
fn coerce_obj(v: &Yaml) -> Yaml {
    if let Yaml::String(s) = v {
        if let Ok(parsed) = serde_yaml::from_str::<Yaml>(s) {
            if parsed.is_mapping() {
                return parsed;
            }
        }
    }
    v.clone()
}

fn os_matches(declared: &str, host: &str) -> bool {
    fn canon(s: &str) -> String {
        match s.to_ascii_lowercase().as_str() {
            "darwin" | "macos" | "osx" | "mac" => "macos".to_string(),
            "win32" | "win" | "windows" => "windows".to_string(),
            other => other.to_string(),
        }
    }
    canon(declared) == canon(host)
}

/// Whether `name` resolves to an executable: an absolute/relative path that
/// exists, or a bare name found on `PATH`.
fn bin_exists(name: &str) -> bool {
    if name.contains('/') || name.contains('\\') {
        return std::path::Path::new(name).exists();
    }
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    std::env::split_paths(&path).any(|dir| {
        let p = dir.join(name);
        p.is_file() || (cfg!(windows) && dir.join(format!("{name}.exe")).is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_classic_fields() {
        let content = "---\nname: my-skill\ndescription: A test skill\nallowed-tools: Bash, Read\nversion: \"1.0\"\n---\n\n# Body\n";
        let meta = parse_skill_metadata(content, "fallback", "fallback desc");
        assert_eq!(meta.name, "my-skill");
        assert_eq!(meta.description, "A test skill");
        assert_eq!(meta.allowed_tools, vec!["Bash", "Read"]);
        assert_eq!(meta.version, Some("1.0".into()));
        assert!(meta.user_invocable);
    }

    #[test]
    fn parses_use_mode() {
        // Default is Trigger when omitted.
        let d = parse_skill_metadata("---\nname: s\ndescription: d\n---\n", "f", "f");
        assert_eq!(d.use_mode, SkillUseMode::Trigger);
        // `use: always` (and aliases) → Always; anything else → Trigger.
        let a = parse_skill_metadata("---\nname: s\ndescription: d\nuse: always\n---\n", "f", "f");
        assert_eq!(a.use_mode, SkillUseMode::Always);
        let t = parse_skill_metadata(
            "---\nname: s\ndescription: d\nuse: trigger\n---\n",
            "f",
            "f",
        );
        assert_eq!(t.use_mode, SkillUseMode::Trigger);
        let alias = parse_skill_metadata(
            "---\nname: s\ndescription: d\nuse-mode: Persistent\n---\n",
            "f",
            "f",
        );
        assert_eq!(alias.use_mode, SkillUseMode::Always);
    }

    #[test]
    fn parses_triggers_sequence_and_string() {
        let seq = parse_skill_metadata(
            "---\nname: w\ndescription: d\ntriggers: [weather, 天气, forecast]\n---\n",
            "f",
            "f",
        );
        assert_eq!(seq.triggers, vec!["weather", "天气", "forecast"]);

        let csv = parse_skill_metadata(
            "---\nname: w\ndescription: d\ntrigger: weather, forecast\n---\n",
            "f",
            "f",
        );
        assert_eq!(csv.triggers, vec!["weather", "forecast"]);
    }

    #[test]
    fn parses_nested_openclaw_metadata() {
        let content = "---\nname: s\ndescription: d\nmetadata:\n  openclaw:\n    os: [darwin, linux]\n    primaryEnv: API_KEY\n    requires:\n      env: [API_KEY]\n      bins: [jq]\n      anyBins: [curl, wget]\n---\n";
        let meta = parse_skill_metadata(content, "f", "f");
        assert_eq!(meta.os, vec!["darwin", "linux"]);
        assert_eq!(meta.primary_env, Some("API_KEY".into()));
        assert_eq!(meta.requires_env, vec!["API_KEY"]);
        assert_eq!(meta.requires_bins, vec!["jq"]);
        assert_eq!(meta.requires_any_bins, vec!["curl", "wget"]);
    }

    #[test]
    fn parses_single_line_json_openclaw_metadata() {
        // OpenClaw's parser only supports single-line frontmatter keys, so it
        // stores metadata as a one-line JSON object.
        let content = "---\nname: s\ndescription: d\nmetadata.openclaw: '{\"os\":[\"darwin\"],\"requires\":{\"bins\":[\"jq\"],\"env\":[\"API_KEY\"]}}'\n---\n";
        let meta = parse_skill_metadata(content, "f", "f");
        assert_eq!(meta.os, vec!["darwin"]);
        assert_eq!(meta.requires_bins, vec!["jq"]);
        assert_eq!(meta.requires_env, vec!["API_KEY"]);
    }

    #[test]
    fn parses_params() {
        let content = "---\nname: s\ndescription: d\nparams:\n  - name: city\n    type: string\n    required: true\n    description: The city to query\n  - name: units\n    type: string\n---\n";
        let meta = parse_skill_metadata(content, "f", "f");
        assert_eq!(meta.params.len(), 2);
        assert_eq!(meta.params[0].name, "city");
        assert!(meta.params[0].required);
        assert_eq!(
            meta.params[0].description.as_deref(),
            Some("The city to query")
        );
        assert_eq!(meta.params[1].type_, "string");
        assert!(!meta.params[1].required);
    }

    #[test]
    fn gating_os_mismatch() {
        let mut meta = SkillMetadata {
            name: "s".into(),
            ..Default::default()
        };
        // Pick an OS that is definitely not the host.
        meta.os = vec![if std::env::consts::OS == "linux" {
            "win32".into()
        } else {
            "linux".into()
        }];
        assert!(meta.ineligible_reason().is_some());
        assert!(!meta.is_eligible());
    }

    #[test]
    fn gating_missing_bin() {
        let meta = SkillMetadata {
            name: "s".into(),
            requires_bins: vec!["this-binary-does-not-exist-xyz".into()],
            ..Default::default()
        };
        assert!(meta.ineligible_reason().unwrap().contains("binary"));
    }

    #[test]
    fn gating_eligible_when_no_requirements() {
        let meta = SkillMetadata {
            name: "s".into(),
            ..Default::default()
        };
        assert!(meta.is_eligible());
    }

    #[test]
    fn extract_body_strips_frontmatter() {
        let content = "---\nname: s\ndescription: d\n---\n\n# Hello\nworld\n";
        assert_eq!(extract_body(content), "# Hello\nworld");
    }
}

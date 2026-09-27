//! ToolSearch — discover deferred tools by keyword.
//!
//! Mirrors the `ToolSearchTool` pattern in `yasasbanukaofficial/claude-code`:
//! tools marked `should_defer() = true` are excluded from the initial tool
//! list sent to the LLM each turn (saves ~80% of tool-definition tokens).
//! The LLM then calls this tool with a query to find and load specialized
//! tools on demand.
//!
//! Result format: full tool schemas (name, description, input_schema) so the
//! LLM can call them directly in subsequent turns — no separate "load" step
//! needed; the next prompt will include the discovered tools automatically.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

const DEFAULT_MAX_RESULTS: usize = 5;
const MAX_RESULTS_HARD_CAP: usize = 20;
const SELECT_PREFIX: &str = "select:";
/// How many of a server's tools a `select:` name that resolves to nothing
/// loads in its place. Five, not three: `ssh_connect` shares its word with
/// five ssh-manager tools that score alike, and at three the one models
/// actually go on to call (`ssh_start_connect_id`) was cut.
const CLOSEST_LIMIT: usize = 5;
/// How many of that server's tool names the answer lists.
const FAMILY_LIST_LIMIT: usize = 30;

/// The server every built-in registers under when they run bundled in one
/// process (`mcp.bundled`, the default): the engine strips `senclaw-` from
/// [`crate::mcp::core_server::SERVER_NAME`], so `browser_search` reaches the
/// model as `mcp__core__browser_search`.
pub const BUNDLED_SERVER: &str = "core";

/// Normalize alternate MCP naming schemes to the canonical bridge form.
/// e.g. `mcp__senclaw-browser__browser_search` → `mcp__browser__search`
pub fn normalize_mcp_tool_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("mcp__senclaw-") {
        if let Some((server, tool)) = rest.split_once("__") {
            let prefix = format!("{server}_");
            let clean_tool = tool.strip_prefix(&prefix).unwrap_or(tool);
            return format!("mcp__{server}__{clean_tool}");
        }
    }
    name.to_string()
}

fn mcp_name_parts(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    rest.split_once("__")
}

/// Canonicalize a tool name for hyphen/underscore-insensitive comparison.
///
/// Models frequently emit `mcp__ssh-manager_mcp__foo` (or all underscores) for
/// a server registered as `ssh-manager-mcp`. The MCP bridge keeps hyphens in
/// the server segment, so an exact match misses. Folding `-` to `_` lets a tool
/// call resolve regardless of which separator the model chose.
fn canonical_tool_name(name: &str) -> String {
    name.replace('-', "_")
}

fn strip_senclaw(server: &str) -> &str {
    server
        .strip_prefix("senclaw-")
        .or_else(|| server.strip_prefix("senclaw_"))
        .unwrap_or(server)
}

/// Does any tool in `tools` belong to MCP server `server` (hyphen-insensitive)?
fn server_registered(server: &str, tools: &[Arc<dyn Tool>]) -> bool {
    let want = format!("mcp__{}__", canonical_tool_name(server));
    tools
        .iter()
        .any(|t| canonical_tool_name(t.name()).starts_with(&want))
}

/// A built-in tool named for the other server layout.
///
/// Built-ins register as `mcp__core__browser_search` when bundled (the
/// default) and as `mcp__browser__search` when each runs as its own server,
/// while skills, transcripts and models also write the registry's
/// `mcp__senclaw-browser__browser_search`. No other stage bridges the two
/// layouts: after bundling shipped, 43 of the 53 `select:` calls that loaded
/// nothing named a tool that was registered under its bundled name.
fn resolve_across_layouts(name: &str, tools: &[Arc<dyn Tool>]) -> Option<Arc<dyn Tool>> {
    let (server, tool) = mcp_name_parts(name)?;
    if tool.is_empty() {
        return None;
    }
    let find = |n: &str| tools.iter().find(|t| t.name() == n).map(Arc::clone);
    if server == BUNDLED_SERVER {
        // Bundled spelling, per-server registration: `browser_search` is
        // `mcp__browser__search`; `bash_run` is `mcp__js__bash_run`.
        let parts = |t: &&Arc<dyn Tool>| {
            let norm = normalize_mcp_tool_name(t.name());
            mcp_name_parts(&norm)
                .filter(|(d, _)| *d != BUNDLED_SERVER)
                .map(|(d, v)| (format!("{d}_{v}"), v.to_string()))
        };
        if let Some(t) = tools.iter().find(|t| parts(t).is_some_and(|(dv, _)| dv == tool)) {
            return Some(Arc::clone(t));
        }
        let mut by_verb = tools.iter().filter(|t| parts(t).is_some_and(|(_, v)| v == tool));
        let first = by_verb.next()?;
        return by_verb.next().is_none().then(|| Arc::clone(first));
    }
    let domain = strip_senclaw(server);
    if server_registered(server, tools) || server_registered(domain, tools) {
        // A real server by that name: the tool is missing, not misspelled.
        return None;
    }
    let verb = tool.strip_prefix(&format!("{domain}_")).unwrap_or(tool);
    find(&format!("mcp__{BUNDLED_SERVER}__{domain}_{verb}"))
        .or_else(|| find(&format!("mcp__{BUNDLED_SERVER}__{tool}")))
}

/// The tool family a query word or a server segment names, in canonical form:
/// `senclaw-browser`, `mcp__senclaw-browser`, `mcp__browser__` and `browser`
/// all name the browser tools. `None` for a full tool name.
fn family_term(term: &str) -> Option<String> {
    let t = term.strip_prefix("mcp__").unwrap_or(term);
    let t = match t.split_once("__") {
        Some((server, "")) => server,
        Some(_) => return None,
        None => t,
    };
    let t = strip_senclaw(t);
    (!t.is_empty()).then(|| canonical_tool_name(t))
}

/// Whether `name` is one of `family`'s tools, whichever layout registered it
/// — `mcp__core__browser_search`, `mcp__browser__search` and
/// `mcp__senclaw-browser__browser_search` are all in `browser`. With `loose`,
/// an app's `mcp__clock-mcp__…` is in `clock` as well as in `clock-mcp`.
fn in_family(name: &str, family: &str, loose: bool) -> bool {
    let norm = canonical_tool_name(&normalize_mcp_tool_name(name));
    let Some((server, tool)) = mcp_name_parts(&norm) else {
        return false;
    };
    let server = strip_senclaw(server);
    if server == family || (loose && server.strip_suffix("_mcp") == Some(family)) {
        return true;
    }
    server == BUNDLED_SERVER && tool.starts_with(&format!("{family}_"))
}

/// What a `select:` name that resolves to nothing is closest to.
struct Closest {
    /// The server/family the name points at, as the model wrote it
    /// (`clock-mcp`, `workspace`).
    label: String,
    /// Loaded in its place, best first — only tools that share a word with the
    /// requested name, or the whole family when it is this small.
    near: Vec<Arc<dyn Tool>>,
    /// The family's tool names (capped) so the model can select exactly.
    names: Vec<String>,
    total: usize,
}

/// The tools closest to a `select:` name that resolves to nothing: the
/// requested server's tools, ranked by the words their names and descriptions
/// share with the requested one. Models guess names
/// (`mcp__clock-mcp__clock_now`) for servers they know exist; loading that
/// server's nearest tools turns a wasted round trip into a usable answer.
/// Nothing is guessed across servers — `None` when the name points at none.
fn closest_tools(name: &str, pool: &[Arc<dyn Tool>]) -> Option<Closest> {
    let members_of = |family: &str| -> Vec<&Arc<dyn Tool>> {
        pool.iter().filter(|t| in_family(t.name(), family, true)).collect()
    };
    let (label, family, wanted, mut members) = match mcp_name_parts(name) {
        Some((server, tool)) => {
            let family = family_term(server)?;
            let members = members_of(&family);
            (strip_senclaw(server).to_string(), family, tool, members)
        }
        // A bare name is a server (`senclaw-workspace`) or a tool whose first
        // word is its server (`weather_forecast` → `weather-mcp`).
        None => {
            let whole = family_term(name)?;
            let whole_members = members_of(&whole);
            if whole_members.is_empty() {
                let first = strip_senclaw(name).split(['_', '-']).next().unwrap_or(name);
                let family = family_term(first)?;
                let members = members_of(&family);
                (first.to_string(), family, name, members)
            } else {
                (strip_senclaw(name).to_string(), whole, name, whole_members)
            }
        }
    };
    members.sort_by(|a, b| a.name().cmp(b.name()));
    let family_words: Vec<&str> = family.split('_').collect();
    let words: Vec<String> = canonical_tool_name(&wanted.to_lowercase())
        .split('_')
        .filter(|w| w.len() >= 3 && !family_words.contains(w) && !["senclaw", "mcp"].contains(w))
        .map(str::to_string)
        .collect();
    let mut scored: Vec<(usize, &Arc<dyn Tool>)> = members
        .iter()
        .map(|t| {
            let tool_part = t.name().rsplit("__").next().unwrap_or(t.name()).to_lowercase();
            let desc = t.description().to_lowercase();
            let score = words
                .iter()
                .map(|w| 2 * usize::from(tool_part.contains(w.as_str())) + usize::from(desc.contains(w.as_str())))
                .sum();
            (score, *t)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name().cmp(b.1.name())));
    let mut near: Vec<Arc<dyn Tool>> = scored
        .into_iter()
        .take(CLOSEST_LIMIT)
        .map(|(_, t)| Arc::clone(t))
        .collect();
    if near.is_empty() && members.len() <= CLOSEST_LIMIT {
        near = members.iter().map(|t| Arc::clone(t)).collect();
    }
    Some(Closest {
        label,
        near,
        names: members
            .iter()
            .take(FAMILY_LIST_LIMIT)
            .map(|t| t.name().to_string())
            .collect(),
        total: members.len(),
    })
}

/// Every `mcp__<server>__<tool>` written out in `text`, first occurrence
/// first — what a skill's instructions tell the model to call.
pub fn mcp_mentions(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(pos) = text[from..].find("mcp__") {
        let start = from + pos;
        let mut end = start;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'-')
        {
            end += 1;
        }
        let mention = text[start..end].trim_end_matches(['_', '-']);
        if mcp_name_parts(mention).is_some_and(|(s, t)| !s.is_empty() && !t.is_empty())
            && !out.iter().any(|m| m == mention)
        {
            out.push(mention.to_string());
        }
        from = end.max(start + 5);
    }
    out
}

/// Resolve a tool by exact name, alias, or normalized MCP alias.
///
/// Stage 0 consults the configured alias map (Plugins → Alias) BEFORE exact
/// matching, so an alias equal to a registered tool name overrides that tool:
/// the call is rewritten to the alias target and the original implementation
/// is shadowed. When the target can't be found (app off, bad name) resolution
/// falls back to the original name so an alias never bricks a working tool.
pub fn resolve_tool_by_name(name: &str, tools: &[Arc<dyn Tool>]) -> Option<Arc<dyn Tool>> {
    if let Some(target) = crate::tools::tool_alias::resolve_alias(name) {
        if target != name {
            if let Some(t) = resolve_tool_ignoring_aliases(&target, tools) {
                return Some(t);
            }
            tracing::warn!(
                "tool alias '{name}' → '{target}': target not registered, falling back to the original name"
            );
        }
    }
    resolve_tool_ignoring_aliases(name, tools)
}

/// The pre-alias resolution cascade. Used directly by the alias layer itself
/// (to locate a target without re-entering the alias map) — everything else
/// should call [`resolve_tool_by_name`].
pub(crate) fn resolve_tool_ignoring_aliases(
    name: &str,
    tools: &[Arc<dyn Tool>],
) -> Option<Arc<dyn Tool>> {
    if let Some(t) = tools.iter().find(|t| t.name() == name) {
        return Some(Arc::clone(t));
    }
    let normalized = normalize_mcp_tool_name(name);
    if normalized != name {
        if let Some(t) = tools.iter().find(|t| t.name() == normalized) {
            return Some(Arc::clone(t));
        }
    }
    // Bridge the stripped form against the registered full form. Tools register
    // under their full server prefix (`mcp__senclaw-browser__browser_search`),
    // but the model — and the skill docs — call them by the stripped bridge
    // form (`mcp__browser__search`). Normalizing the tool's OWN name too makes
    // the two meet, so the documented short name resolves to whatever long name
    // the manager actually registered.
    if let Some(t) = tools
        .iter()
        .find(|t| normalize_mcp_tool_name(t.name()) == normalized)
    {
        return Some(Arc::clone(t));
    }
    for t in tools {
        if t.aliases()
            .iter()
            .any(|a| *a == name || normalize_mcp_tool_name(a) == normalized)
        {
            return Some(Arc::clone(t));
        }
    }
    // A tool renamed by a configured alias (Plugins → Alias) still resolves
    // by its original registered name — old transcripts, skill docs, and
    // hardcoded tool lists keep working after a rename.
    for t in tools {
        if let Some(orig) = t.renamed_from() {
            if orig == name || normalize_mcp_tool_name(orig) == normalized {
                return Some(Arc::clone(t));
            }
        }
    }
    // Hyphen/underscore-insensitive match: `mcp__ssh-manager_mcp__x` should
    // resolve to a tool registered as `mcp__ssh-manager-mcp__x`.
    let canon = canonical_tool_name(&normalized);
    if let Some(t) = tools
        .iter()
        .find(|t| canonical_tool_name(t.name()) == canon)
    {
        return Some(Arc::clone(t));
    }
    for t in tools {
        if t.aliases().iter().any(|a| canonical_tool_name(a) == canon) {
            return Some(Arc::clone(t));
        }
    }
    if let Some(t) = resolve_across_layouts(name, tools) {
        return Some(t);
    }
    // Last resort: match MCP server + verb suffix (handles unstripped names).
    if let Some((server, verb)) = mcp_name_parts(&normalized) {
        let needle = format!("__{verb}");
        let canon_server = canonical_tool_name(server);
        tools
            .iter()
            .find(|t| {
                let n = t.name();
                n.ends_with(&needle)
                    && (canonical_tool_name(n).contains(&format!("mcp__{canon_server}__"))
                        || canonical_tool_name(n)
                            .contains(&format!("mcp__senclaw_{canon_server}__")))
            })
            .map(Arc::clone)
    } else {
        // Bare name without `mcp__` prefix — models sometimes strip the
        // `mcp__{server}__` prefix and emit just the verb or server+verb:
        //   - `event_create` for `mcp__space__event_create` (verb only)
        //   - `space_event_create` for `mcp__space__event_create` (server_verb)
        let canon_bare = canonical_tool_name(&normalized);

        // Strategy 1: exact verb match — bare name IS the verb segment.
        // e.g. `event_create` → unique tool ending with `__event_create`.
        let suffix = format!("__{canon_bare}");
        let matches: Vec<_> = tools
            .iter()
            .filter(|t| canonical_tool_name(t.name()).ends_with(&suffix))
            .collect();
        if matches.len() == 1 {
            return Some(Arc::clone(matches[0]));
        }

        // Strategy 2: server_verb concatenation — model concatenated server
        // and verb with `_` instead of `mcp__{server}__{verb}`.
        // e.g. `space_event_create` → `mcp__space__event_create` where
        // server=space, verb=event_create.
        let matches: Vec<_> = tools
            .iter()
            .filter(|t| {
                let norm = normalize_mcp_tool_name(t.name());
                if let Some((server, verb)) = mcp_name_parts(&norm) {
                    let concat = format!(
                        "{}_{}",
                        canonical_tool_name(server),
                        canonical_tool_name(verb)
                    );
                    concat == canon_bare
                } else {
                    false
                }
            })
            .collect();
        if matches.len() == 1 {
            Some(Arc::clone(matches[0]))
        } else {
            None
        }
    }
}

fn parse_select_names(query: &str) -> Vec<String> {
    query[SELECT_PREFIX.len()..]
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn select_matches(names: &[String], tools: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
    let mut out = Vec::new();
    for name in names {
        if let Some(t) = resolve_tool_by_name(name, tools) {
            if !out.iter().any(|x: &Arc<dyn Tool>| x.name() == t.name()) {
                out.push(t);
            }
        }
    }
    out
}

/// Closure that returns the full list of currently deferred tools. Engine
/// supplies this so `ToolSearch` always sees the live registry.
pub type DeferredToolsFn = Arc<dyn Fn() -> Vec<Arc<dyn Tool>> + Send + Sync>;

/// Closure returning EVERY tool the agent could invoke this turn (active +
/// deferred, after `use_tools` / Plan / DAG filters). `select:` resolves names
/// against this superset — not just the deferred subset — so naming an
/// already-active tool (e.g. `Skill`) confirms availability instead of the
/// misleading "0 matches" that made the model conclude the tool didn't exist
/// and keep thrashing.
pub type AllToolsFn = Arc<dyn Fn() -> Vec<Arc<dyn Tool>> + Send + Sync>;

/// Closure that registers a tool name as "discovered" — the engine then
/// includes it in the active tool list for subsequent LLM turns. Without
/// this, the model can read schemas but can't actually invoke the tool.
pub type RegisterDiscoveredFn = Arc<dyn Fn(&str) + Send + Sync>;

/// A skill exposed to ToolSearch so keyword discovery surfaces SKILLS too, not
/// just deferred tools. Skills aren't tools — they're loaded via the `Skill`
/// tool — but users expect `ToolSearch("ssh")` to find `ssh-connect` etc. when
/// there are ssh skills installed. Without this, a query only searches deferred
/// tools (often zero), so skills are invisible to keyword discovery.
#[derive(Clone)]
pub struct SkillSearchRow {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub triggers: Vec<String>,
}

/// Closure returning the live list of model-invocable skills.
pub type SkillsFn = Arc<dyn Fn() -> Vec<SkillSearchRow> + Send + Sync>;

pub struct ToolSearchTool {
    deferred_resolver: DeferredToolsFn,
    register_discovered: Option<RegisterDiscoveredFn>,
    skills_resolver: Option<SkillsFn>,
    all_tools_resolver: Option<AllToolsFn>,
}

impl ToolSearchTool {
    pub fn new(deferred_resolver: DeferredToolsFn) -> Self {
        Self {
            deferred_resolver,
            register_discovered: None,
            skills_resolver: None,
            all_tools_resolver: None,
        }
    }

    /// Inject the skill resolver so keyword searches also match installed
    /// skills (returned with a hint to invoke them via the `Skill` tool).
    pub fn with_skills(mut self, resolver: SkillsFn) -> Self {
        self.skills_resolver = Some(resolver);
        self
    }

    /// Inject the "all available tools" resolver used by the `select:` path so
    /// it can resolve already-active tools (not just deferred ones). Without it,
    /// `select:` falls back to the deferred pool and a `select:<active-tool>`
    /// reports "0 matches" even though the tool is loaded and callable.
    pub fn with_all_tools(mut self, resolver: AllToolsFn) -> Self {
        self.all_tools_resolver = Some(resolver);
        self
    }

    /// Rank skills by keyword overlap with name / triggers / when-to-use /
    /// description — mirrors [`rank_matches`] but for the skill registry.
    fn rank_skills(query: &str, skills: &[SkillSearchRow], limit: usize) -> Vec<SkillSearchRow> {
        let q_lower = query.to_lowercase();
        let q_terms: Vec<&str> = q_lower
            .split_whitespace()
            .filter(|t| !t.is_empty())
            .collect();
        if q_terms.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(i32, SkillSearchRow)> = skills
            .iter()
            .filter_map(|s| {
                let name = s.name.to_lowercase();
                let desc = s.description.to_lowercase();
                let when = s.when_to_use.as_deref().unwrap_or("").to_lowercase();
                let trigs = s.triggers.join(" ").to_lowercase();
                let mut score = 0i32;
                for term in &q_terms {
                    if name.contains(term) {
                        score += 100;
                    }
                    if trigs.contains(term) {
                        score += 40;
                    }
                    if when.contains(term) {
                        score += 25;
                    }
                    if desc.contains(term) {
                        score += 10;
                    }
                }
                if score > 0 {
                    Some((score, s.clone()))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        scored.into_iter().take(limit).map(|(_, s)| s).collect()
    }

    /// Inject the discovery callback. Engine calls this immediately after
    /// constructing the tool so each search result is auto-loaded for the
    /// rest of the session.
    pub fn with_discovery(mut self, cb: RegisterDiscoveredFn) -> Self {
        self.register_discovered = Some(cb);
        self
    }

    fn rank_matches(query: &str, tools: &[Arc<dyn Tool>], limit: usize) -> Vec<Arc<dyn Tool>> {
        let q_lower = query.to_lowercase();
        let q_terms: Vec<&str> = q_lower
            .split_whitespace()
            .filter(|t| !t.is_empty())
            .collect();
        if q_terms.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(i32, Arc<dyn Tool>)> = tools
            .iter()
            .filter_map(|t| {
                let name = t.name().to_lowercase();
                let hint = t.search_hint().to_lowercase();
                let desc = t.description().to_lowercase();
                let mut score = 0i32;
                // Boost entire MCP server families when the query names a server
                // (e.g. "browser search" → every browser tool). `in_family`
                // spans both layouts — `mcp__core__browser_*` bundled,
                // `mcp__browser__*` per server — and the query may spell the
                // server as the registry does: `senclaw-browser` matched nothing
                // at all before, and models sent it 15 times after bundling.
                for term in &q_terms {
                    if family_term(term).is_some_and(|f| in_family(&name, &f, false)) {
                        score += 80;
                    }
                }

                for term in &q_terms {
                    // Highest weight: exact name substring (e.g. user asks "screenshot" → "browser_screenshot")
                    if name.contains(term) {
                        score += 100;
                    }
                    if hint.contains(term) {
                        score += 25;
                    }
                    if desc.contains(term) {
                        score += 5;
                    }
                    for alias in t.aliases() {
                        if alias.to_lowercase().contains(term) {
                            score += 60;
                        }
                    }
                }
                if score > 0 {
                    Some((score, Arc::clone(t)))
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| {
            // higher score first; then alphabetical name for cache-stable order
            b.0.cmp(&a.0).then_with(|| a.1.name().cmp(b.1.name()))
        });
        scored.into_iter().take(limit).map(|(_, t)| t).collect()
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn description(&self) -> &str {
        "Search for specialized tools AND skills that aren't loaded by default. \
         Returns full schemas of matching tools (callable in subsequent turns) \
         and matching skills (invoke via the `Skill` tool). Use when a task needs \
         capabilities beyond the core toolset (e.g. browser screenshots, calendar \
         events, code graph queries, or an installed skill like 'ssh')."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords describing the capability you need. Examples: 'browser screenshot', 'calendar event', 'wiki search', 'code graph symbols'."
                },
                "max_results": {
                    "type": "number",
                    "description": "Max tools to return (default 5, hard cap 20)."
                }
            },
            "required": ["query"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn always_load(&self) -> bool {
        // ToolSearch is the discovery mechanism itself — must be in every prompt.
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        let q = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
        if q.trim().is_empty() {
            return Err("query is required".to_string());
        }
        Ok(())
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let limit = input
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX_RESULTS as u64)
            .min(MAX_RESULTS_HARD_CAP as u64) as usize;

        let deferred = (self.deferred_resolver)();
        let is_select = query.starts_with(SELECT_PREFIX);

        // `select:` resolves against the FULL available toolset (active +
        // deferred), not just the deferred subset. Selecting an already-active
        // tool (e.g. `Skill`) must confirm it's callable instead of returning a
        // misleading "0 matches" that makes the model conclude the tool doesn't
        // exist and keep retrying. Falls back to `deferred` if the engine didn't
        // wire an all-tools resolver (e.g. in unit tests).
        let select_pool: Vec<Arc<dyn Tool>> = if is_select {
            match &self.all_tools_resolver {
                Some(r) => r(),
                None => deferred.clone(),
            }
        } else {
            Vec::new()
        };

        let requested_names: Vec<String> = if is_select {
            parse_select_names(&query)
        } else {
            Vec::new()
        };

        let mut matches = if is_select {
            if requested_names.is_empty() {
                Vec::new()
            } else {
                select_matches(&requested_names, &select_pool)
            }
        } else {
            Self::rank_matches(&query, &deferred, limit)
        };
        let exact_count = matches.len();

        // Names the model asked for that resolve to NO registered tool. Reported
        // with actionable guidance so the model stops guessing tool spellings.
        let not_found: Vec<String> = requested_names
            .iter()
            .filter(|n| resolve_tool_by_name(n, &select_pool).is_none())
            .cloned()
            .collect();

        // A name that resolves to nothing still names a server the model
        // believes in: load that server's closest tools instead of nothing, so
        // the next turn can call one rather than search again.
        let closest: Vec<(&String, Option<Closest>)> = not_found
            .iter()
            .map(|n| (n, closest_tools(n, &select_pool)))
            .collect();
        for c in closest.iter().filter_map(|(_, c)| c.as_ref()) {
            for t in &c.near {
                if !matches.iter().any(|m| m.name() == t.name()) {
                    matches.push(Arc::clone(t));
                }
            }
        }

        // Which resolved matches were deferred (freshly loaded) vs already
        // active (a no-op confirm). Drives the per-tool status in the summary.
        let deferred_names: std::collections::HashSet<String> =
            deferred.iter().map(|t| t.name().to_string()).collect();

        // Also search SKILLS (keyword queries only — `select:` loads tools by
        // exact name). Skills are invoked via the `Skill` tool, not "loaded",
        // so they're surfaced separately with an invocation hint.
        let skill_matches: Vec<SkillSearchRow> = if is_select {
            Vec::new()
        } else if let Some(ref sr) = self.skills_resolver {
            Self::rank_skills(&query, &sr(), limit)
        } else {
            Vec::new()
        };

        // Register each match as discovered — engine will include them in
        // subsequent `tools_for_main_agent()` calls. Without this, the model
        // gets the schema here but can't actually call the tool next turn.
        if let Some(ref cb) = self.register_discovered {
            for t in &matches {
                cb(t.name());
            }
        }

        let payload: Vec<Value> = matches
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "input_schema": t.input_schema(),
                })
            })
            .collect();

        let skills_payload: Vec<Value> = skill_matches
            .iter()
            .map(|s| {
                serde_json::json!({
                    "skill": s.name,
                    "description": s.description,
                    "invoke": format!("Skill {{ \"skill\": \"{}\" }}", s.name),
                })
            })
            .collect();

        let text_summary = if is_select {
            // Explicit load-by-name path. Report loaded tools (distinguishing
            // freshly-discovered from already-active) and give the model a way
            // forward for names that resolve to nothing.
            let mut s = String::new();
            if exact_count > 0 {
                s.push_str(&format!("Loaded {exact_count} tool(s):\n"));
                for t in &matches[..exact_count] {
                    let status = if deferred_names.contains(t.name()) {
                        "now available"
                    } else {
                        "already available — call it directly"
                    };
                    s.push_str(&format!("  - {} ({})\n", t.name(), status));
                }
                s.push_str("Call them directly in your next turn.\n");
            }
            let mut unanswered: Vec<&str> = Vec::new();
            let mut missing_families: Vec<String> = Vec::new();
            for (name, c) in &closest {
                match c {
                    Some(c) if c.total > 0 => {
                        s.push_str(&format!("\nNo tool is named `{name}`."));
                        if c.near.is_empty() {
                            s.push('\n');
                        } else {
                            s.push_str(&format!(
                                " Closest in `{}` — loaded, call one directly if it fits:\n",
                                c.label
                            ));
                            for t in &c.near {
                                let line = t.description().lines().next().unwrap_or("");
                                let line: String = line.chars().take(120).collect();
                                s.push_str(&format!("  - {}: {line}\n", t.name()));
                            }
                        }
                        let more = c.total.saturating_sub(c.names.len());
                        s.push_str(&format!(
                            "All {} tool(s) in `{}`: {}{}. Load another with `select:<name>`.\n",
                            c.total,
                            c.label,
                            c.names.join(", "),
                            if more > 0 { format!(", +{more} more") } else { String::new() },
                        ));
                    }
                    Some(c) => {
                        unanswered.push(name.as_str());
                        missing_families.push(c.label.clone());
                    }
                    None => unanswered.push(name.as_str()),
                }
            }
            if !unanswered.is_empty() {
                s.push_str(&format!(
                    "\nNo registered tool for: {}.\n",
                    unanswered.join(", ")
                ));
                missing_families.dedup();
                for family in &missing_families {
                    s.push_str(&format!(
                        "Nothing from `{family}` is registered in this session — the app may not be installed, or its MCP server failed to start.\n"
                    ));
                }
                s.push_str(
                    "These names don't resolve to any tool. If they came from a skill's \
                     instructions, run the skill itself with the `Skill` tool \
                     (e.g. `Skill {\"skill\": \"ssh-connect\"}`) — `Skill` is always loaded, \
                     so never ToolSearch for `Skill`. If a real tool is missing, its MCP \
                     server may not be installed; fall back to `Bash` or another available tool.\n",
                );
            }
            if matches.is_empty() && not_found.is_empty() {
                s.push_str("No tool names given to select. Use `select:name1,name2`.\n");
            }
            s
        } else if matches.is_empty() && skill_matches.is_empty() {
            format!(
                "No tools or skills matched query '{query}'. {} deferred tools available — try broader keywords.",
                deferred.len()
            )
        } else {
            let mut s = String::new();
            if !matches.is_empty() {
                s.push_str(&format!(
                    "Found {} tool(s) matching '{}':\n",
                    matches.len(),
                    query
                ));
                for t in &matches {
                    s.push_str(&format!("  - {}: {}\n", t.name(), t.search_hint()));
                }
                s.push_str("These tools are now usable. Call them directly in your next turn.\n");
            }
            if !skill_matches.is_empty() {
                s.push_str(&format!(
                    "\nFound {} skill(s) matching '{}' — invoke with the `Skill` tool (already loaded, do not ToolSearch for it):\n",
                    skill_matches.len(),
                    query
                ));
                for sk in &skill_matches {
                    s.push_str(&format!("  - {}: {}\n", sk.name, sk.description));
                }
                s.push_str(
                    "Load one with `Skill {\"skill\": \"<name>\"}` before doing the task.\n",
                );
            }
            s
        };

        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({
                "query": query,
                "matches": payload,
                "skills": skills_payload,
                "deferred_total": deferred.len(),
            }),
            result_for_assistant: text_summary,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let tool_count = data
            .get("matches")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let skill_count = data
            .get("skills")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let count = tool_count + skill_count;
        let query = data
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        ToolResultMessage {
            // The query, not the tool's own name: a step list showing
            // "ToolSearch" four times says nothing about what was looked up.
            title: if query.is_empty() {
                "ToolSearch".to_string()
            } else {
                query.clone()
            },
            summary: format!("{count} matches for '{query}'"),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let q = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
        if q.is_empty() {
            "ToolSearch".to_string()
        } else {
            format!("ToolSearch: \"{}\"", q)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::{Tool, ToolPermissionInfo};
    use std::sync::Mutex;

    /// Stub tool used by tests — implements the bare minimum.
    struct StubTool {
        name: &'static str,
        desc: &'static str,
        hint: &'static str,
        deferred: bool,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.desc
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type":"object"})
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn call(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
            Ok(vec![])
        }
        fn gen_tool_result_message(&self, _d: &Value, _i: &Value) -> ToolResultMessage {
            ToolResultMessage {
                title: String::new(),
                summary: String::new(),
                content: Value::Null,
            }
        }
        fn get_display_title(&self, _i: &Value) -> String {
            self.name.to_string()
        }
        fn gen_tool_permission(&self, _i: &Value) -> Option<ToolPermissionInfo> {
            None
        }
        fn search_hint(&self) -> String {
            self.hint.to_string()
        }
        fn should_defer(&self) -> bool {
            self.deferred
        }
    }

    fn fixtures() -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(StubTool {
                name: "browser_screenshot",
                desc: "Take a screenshot of the current browser tab.",
                hint: "screenshot browser tab capture",
                deferred: true,
            }),
            Arc::new(StubTool {
                name: "calendar_create",
                desc: "Create a calendar event.",
                hint: "calendar event create",
                deferred: true,
            }),
            Arc::new(StubTool {
                name: "wiki_search",
                desc: "Search the wiki.",
                hint: "wiki search documents",
                deferred: true,
            }),
        ]
    }

    #[test]
    fn rank_matches_prefers_name_hits() {
        let tools = fixtures();
        let hits = ToolSearchTool::rank_matches("screenshot", &tools, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name(), "browser_screenshot");
    }

    #[test]
    fn resolve_tolerates_hyphen_underscore_in_mcp_server() {
        // Tool registered with hyphens in the server segment (Space App MCP).
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(StubTool {
            name: "mcp__ssh-manager-mcp__ssh_list_hosts",
            desc: "List SSH hosts.",
            hint: "ssh list hosts",
            deferred: true,
        })];
        // Model emits underscores instead of hyphens — must still resolve.
        for called in [
            "mcp__ssh-manager-mcp__ssh_list_hosts", // exact
            "mcp__ssh-manager_mcp__ssh_list_hosts", // observed failure
            "mcp__ssh_manager_mcp__ssh_list_hosts", // all underscores
        ] {
            let t = resolve_tool_by_name(called, &tools);
            assert!(t.is_some(), "should resolve {called}");
            assert_eq!(t.unwrap().name(), "mcp__ssh-manager-mcp__ssh_list_hosts");
        }
    }

    #[test]
    fn rank_matches_returns_empty_for_empty_query() {
        let tools = fixtures();
        let hits = ToolSearchTool::rank_matches("", &tools, 5);
        assert!(hits.is_empty());
    }

    #[test]
    fn rank_matches_combines_multi_term_score() {
        let tools = fixtures();
        let hits = ToolSearchTool::rank_matches("calendar event", &tools, 5);
        assert_eq!(hits.first().map(|t| t.name()), Some("calendar_create"));
    }

    #[test]
    fn rank_matches_caps_at_limit() {
        let tools = fixtures();
        let hits = ToolSearchTool::rank_matches("create event search", &tools, 1);
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn call_returns_serialized_matches() {
        let resolver: DeferredToolsFn = Arc::new(|| fixtures());
        let tool = ToolSearchTool::new(resolver);
        let ctx = ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let out = tool
            .call(serde_json::json!({"query": "screenshot"}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Result { data, .. } = &out[0] else {
            panic!("unexpected variant");
        };
        let matches = data["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["name"], "browser_screenshot");
        assert!(matches[0]["input_schema"].is_object());
    }

    #[tokio::test]
    async fn call_no_match_reports_total_deferred() {
        let resolver: DeferredToolsFn = Arc::new(|| fixtures());
        let tool = ToolSearchTool::new(resolver);
        let ctx = ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let out = tool
            .call(
                serde_json::json!({"query": "nonexistent-feature-xyzqq"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutput::Result {
            data,
            result_for_assistant,
        } = &out[0]
        else {
            panic!();
        };
        assert_eq!(data["matches"].as_array().unwrap().len(), 0);
        assert_eq!(data["deferred_total"], 3);
        assert!(result_for_assistant.contains("No tools or skills matched"));
    }

    #[test]
    fn normalize_mcp_tool_name_strips_senclaw_prefix() {
        assert_eq!(
            super::normalize_mcp_tool_name("mcp__senclaw-browser__browser_search"),
            "mcp__browser__search"
        );
    }

    #[test]
    fn resolve_stripped_bridge_name_to_registered_full_name() {
        // The manager registers MCP tools under the FULL server prefix, but the
        // agent-browser skill (and the model) call them by the stripped bridge
        // form. The documented short name must resolve to the registered tool,
        // both for direct dispatch and for `select:` loading via ToolSearch.
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(StubTool {
                name: "mcp__senclaw-browser__browser_search",
                desc: "Search the web.",
                hint: "browser search web",
                deferred: true,
            }),
            Arc::new(StubTool {
                name: "mcp__senclaw-browser__browser_close_tab",
                desc: "Close a browser tab.",
                hint: "browser close tab",
                deferred: true,
            }),
        ];
        for called in [
            "mcp__browser__search",                 // skill-documented short form
            "mcp__senclaw-browser__browser_search", // registered full form
        ] {
            let t = resolve_tool_by_name(called, &tools);
            assert!(t.is_some(), "should resolve {called}");
            assert_eq!(t.unwrap().name(), "mcp__senclaw-browser__browser_search");
        }
        // The exact `select:` query from the skill must load the tool too.
        let hits = select_matches(
            &[
                "mcp__browser__search".to_string(),
                "mcp__browser__close_tab".to_string(),
            ],
            &tools,
        );
        assert_eq!(hits.len(), 2, "select: should load both stripped names");
    }

    #[test]
    fn select_query_loads_exact_tools() {
        let tools = fixtures();
        let hits = select_matches(
            &["browser_screenshot".to_string(), "wiki_search".to_string()],
            &tools,
        );
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|t| t.name() == "browser_screenshot"));
        assert!(hits.iter().any(|t| t.name() == "wiki_search"));
    }

    #[tokio::test]
    async fn call_select_prefix_registers_tools() {
        let discovered = Arc::new(Mutex::new(Vec::<String>::new()));
        let disc = Arc::clone(&discovered);
        let resolver: DeferredToolsFn = Arc::new(|| fixtures());
        let register: RegisterDiscoveredFn =
            Arc::new(move |name| disc.lock().unwrap().push(name.to_string()));
        let tool = ToolSearchTool::new(resolver).with_discovery(register);
        let ctx = ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let out = tool
            .call(
                serde_json::json!({"query": "select:browser_screenshot,wiki_search"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutput::Result { data, .. } = &out[0] else {
            panic!("unexpected variant");
        };
        assert_eq!(data["matches"].as_array().unwrap().len(), 2);
        let names = discovered.lock().unwrap();
        assert!(names.contains(&"browser_screenshot".to_string()));
        assert!(names.contains(&"wiki_search".to_string()));
    }

    #[test]
    fn rank_matches_boosts_browser_family() {
        let tools = fixtures();
        let hits = ToolSearchTool::rank_matches("browser search", &tools, 5);
        assert_eq!(hits.first().map(|t| t.name()), Some("browser_screenshot"));
    }

    #[test]
    fn resolve_bare_name_server_verb_concat() {
        // Model emits `space_event_create` for `mcp__space__event_create`
        // (server + "_" + verb, no mcp__ prefix).
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(StubTool {
                name: "mcp__space__event_create",
                desc: "Create event",
                hint: "event create",
                deferred: true,
            }),
            Arc::new(StubTool {
                name: "mcp__space__event_delete",
                desc: "Delete event",
                hint: "event delete",
                deferred: true,
            }),
        ];

        // server_verb concatenation: space + _ + event_create
        let t = resolve_tool_by_name("space_event_create", &tools);
        assert!(t.is_some(), "should resolve space_event_create");
        assert_eq!(t.unwrap().name(), "mcp__space__event_create");

        // Also works for other verbs
        let t = resolve_tool_by_name("space_event_delete", &tools);
        assert!(t.is_some(), "should resolve space_event_delete");
        assert_eq!(t.unwrap().name(), "mcp__space__event_delete");

        // Pure verb match (only if unique)
        let single: Vec<Arc<dyn Tool>> = vec![Arc::new(StubTool {
            name: "mcp__space__event_create",
            desc: "Create event",
            hint: "event create",
            deferred: true,
        })];
        let t = resolve_tool_by_name("event_create", &single);
        assert!(t.is_some(), "should resolve bare verb event_create");

        // Ambiguous verb → None (two tools share the suffix)
        let ambig: Vec<Arc<dyn Tool>> = vec![
            Arc::new(StubTool {
                name: "mcp__space__event_create",
                desc: "Create event",
                hint: "",
                deferred: true,
            }),
            Arc::new(StubTool {
                name: "mcp__calendar__event_create",
                desc: "Create event",
                hint: "",
                deferred: true,
            }),
        ];
        let t = resolve_tool_by_name("event_create", &ambig);
        assert!(t.is_none(), "ambiguous verb should return None");

        // But server_verb is still unique even with ambiguous verb
        let t = resolve_tool_by_name("space_event_create", &ambig);
        assert!(
            t.is_some(),
            "server_verb should resolve even when verb alone is ambiguous"
        );
        assert_eq!(t.unwrap().name(), "mcp__space__event_create");
    }

    #[test]
    fn resolve_bare_name_with_senclaw_prefix_tools() {
        // Real-world case: tool registered as mcp__senclaw-space__space_event_create
        // which normalizes to mcp__space__event_create
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(StubTool {
            name: "mcp__senclaw-space__space_event_create",
            desc: "Create event",
            hint: "event create",
            deferred: true,
        })];

        let t = resolve_tool_by_name("space_event_create", &tools);
        assert!(t.is_some(), "should resolve via normalize + server_verb");
        assert_eq!(t.unwrap().name(), "mcp__senclaw-space__space_event_create");
    }

    fn skill_fixtures() -> Vec<SkillSearchRow> {
        vec![
            SkillSearchRow {
                name: "ssh-connect".into(),
                description: "Guide for connecting to SSH servers and running commands.".into(),
                when_to_use: Some("connect to a server over ssh".into()),
                triggers: vec!["ssh connect".into(), "run command on server".into()],
            },
            SkillSearchRow {
                name: "ssh-reporting".into(),
                description: "Report SSH connection status and stats.".into(),
                when_to_use: None,
                triggers: vec!["ssh status".into()],
            },
            SkillSearchRow {
                name: "pdf-maker".into(),
                description: "Create PDF documents.".into(),
                when_to_use: None,
                triggers: vec!["make a pdf".into()],
            },
        ]
    }

    #[test]
    fn rank_skills_matches_by_name_and_triggers() {
        let skills = skill_fixtures();
        let hits = ToolSearchTool::rank_skills("ssh", &skills, 5);
        // Both ssh skills match; the pdf skill does not.
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|s| s.name.starts_with("ssh-")));
    }

    #[tokio::test]
    async fn call_returns_skills_when_no_tools_match() {
        // Zero deferred tools (the reported bug: deferred_total == 0), but the
        // query matches skills — those must still surface.
        let resolver: DeferredToolsFn = Arc::new(Vec::new);
        let skills: SkillsFn = Arc::new(skill_fixtures);
        let tool = ToolSearchTool::new(resolver).with_skills(skills);
        let ctx = ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let out = tool
            .call(serde_json::json!({"query": "ssh"}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Result {
            data,
            result_for_assistant,
        } = &out[0]
        else {
            panic!();
        };
        assert_eq!(data["matches"].as_array().unwrap().len(), 0);
        assert_eq!(data["deferred_total"], 0);
        let skills_arr = data["skills"].as_array().unwrap();
        assert_eq!(skills_arr.len(), 2);
        assert_eq!(skills_arr[0]["skill"], "ssh-connect");
        assert!(skills_arr[0]["invoke"]
            .as_str()
            .unwrap()
            .contains("ssh-connect"));
        assert!(result_for_assistant.contains("skill(s) matching"));
    }

    #[tokio::test]
    async fn select_resolves_already_active_tool_via_all_pool() {
        // Regression: `select:Skill` returned "0 matches" because `select_matches`
        // only searched the DEFERRED pool, and always-loaded tools (Skill,
        // ToolSearch) aren't deferred. With an all-tools resolver, selecting an
        // active tool confirms it instead of dead-ending.
        let active: Arc<dyn Tool> = Arc::new(StubTool {
            name: "Skill",
            desc: "Execute an agent skill",
            hint: "run a skill",
            deferred: false,
        });
        let deferred_only: DeferredToolsFn = Arc::new(fixtures);
        let all: AllToolsFn = Arc::new(move || {
            let mut v = fixtures();
            v.push(Arc::clone(&active));
            v
        });
        let tool = ToolSearchTool::new(deferred_only).with_all_tools(all);
        let ctx = ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let out = tool
            .call(serde_json::json!({"query": "select:Skill"}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Result {
            data,
            result_for_assistant,
        } = &out[0]
        else {
            panic!();
        };
        let matches = data["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["name"], "Skill");
        assert!(
            result_for_assistant.contains("already available"),
            "should tell the model Skill is already callable, got: {result_for_assistant}"
        );
    }

    #[tokio::test]
    async fn select_not_found_gives_actionable_guidance() {
        // `select:mcp__ssh__connect` for a non-existent MCP tool must not just
        // say "0 matches" — it should point the model at the `Skill` tool / Bash.
        let deferred_only: DeferredToolsFn = Arc::new(fixtures);
        let all: AllToolsFn = Arc::new(fixtures);
        let tool = ToolSearchTool::new(deferred_only).with_all_tools(all);
        let ctx = ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let out = tool
            .call(
                serde_json::json!({"query": "select:mcp__ssh__connect"}),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutput::Result {
            data,
            result_for_assistant,
        } = &out[0]
        else {
            panic!();
        };
        assert_eq!(data["matches"].as_array().unwrap().len(), 0);
        assert!(result_for_assistant.contains("No registered tool for"));
        assert!(
            result_for_assistant.contains("Skill"),
            "guidance should mention the Skill tool as the way to run skills"
        );
    }

    fn stub(name: &'static str, desc: &'static str) -> Arc<dyn Tool> {
        Arc::new(StubTool {
            name,
            desc,
            hint: "",
            deferred: true,
        })
    }

    fn ctx() -> ToolContext<'static> {
        ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        }
    }

    /// The built-ins as a bundled install registers them, next to an app that
    /// reuses the browser's tool names.
    fn bundled() -> Vec<Arc<dyn Tool>> {
        vec![
            stub("mcp__core__browser_search", "Search the web."),
            stub("mcp__core__browser_navigate", "Open a URL."),
            stub("mcp__core__space_note_create", "Create a note."),
            stub("mcp__core__bash_run", "Run a shell command."),
            stub("mcp__core__schedule_watch", "Watch a job until it finishes."),
            stub("mcp__mini-browser-mcp__browser_navigate", "Open a URL in the mini browser."),
            stub("mcp__clock-mcp__clock_get_time", "Get the current time now."),
            stub("mcp__clock-mcp__clock_timer_start", "Start a timer."),
        ]
    }

    #[test]
    fn the_bundled_server_is_the_core_servers_name_without_senclaw() {
        assert_eq!(
            format!("senclaw-{BUNDLED_SERVER}"),
            crate::mcp::core_server::SERVER_NAME
        );
    }

    #[test]
    fn a_per_server_name_resolves_to_its_bundled_tool() {
        let tools = bundled();
        for (called, want) in [
            ("mcp__senclaw-browser__browser_search", "mcp__core__browser_search"),
            ("mcp__browser__search", "mcp__core__browser_search"),
            ("mcp__browser__navigate", "mcp__core__browser_navigate"),
            ("mcp__space__note_create", "mcp__core__space_note_create"),
            ("mcp__senclaw-js__bash_run", "mcp__core__bash_run"),
            ("mcp__senclaw-schedule__schedule_watch", "mcp__core__schedule_watch"),
            ("mcp__senclaw_browser__browser_search", "mcp__core__browser_search"),
        ] {
            assert_eq!(resolve_tool_by_name(called, &tools).map(|t| t.name().to_string()).as_deref(), Some(want), "{called}");
        }
        // An installed app keeps its own tools: its missing verb is missing,
        // not the built-in browser's.
        assert!(resolve_tool_by_name("mcp__mini-browser-mcp__browser_search", &tools).is_none());
        assert!(resolve_tool_by_name("mcp__wiki__delete", &tools).is_none());
    }

    #[test]
    fn a_bundled_name_resolves_when_each_server_runs_alone() {
        let tools = vec![
            stub("mcp__browser__search", "Search the web."),
            stub("mcp__js__bash_run", "Run a shell command."),
        ];
        for (called, want) in [
            ("mcp__core__browser_search", "mcp__browser__search"),
            ("mcp__core__bash_run", "mcp__js__bash_run"),
        ] {
            assert_eq!(resolve_tool_by_name(called, &tools).map(|t| t.name().to_string()).as_deref(), Some(want), "{called}");
        }
    }

    #[test]
    fn a_query_naming_the_server_finds_its_tools_in_either_layout() {
        let tools = bundled();
        for query in ["senclaw-browser", "mcp__senclaw-browser", "browser"] {
            let hits = ToolSearchTool::rank_matches(query, &tools, 5);
            let names: Vec<&str> = hits.iter().map(|t| t.name()).collect();
            assert!(names.contains(&"mcp__core__browser_search"), "{query}: {names:?}");
            assert!(names.contains(&"mcp__core__browser_navigate"), "{query}: {names:?}");
        }
        let hits = ToolSearchTool::rank_matches("senclaw-browser", &tools, 5);
        assert!(
            hits.iter().all(|t| t.name().starts_with("mcp__core__browser_")),
            "the server name alone boosts only that server"
        );
    }

    #[tokio::test]
    async fn a_select_that_misses_loads_that_servers_closest_tools() {
        let discovered = Arc::new(Mutex::new(Vec::<String>::new()));
        let disc = Arc::clone(&discovered);
        let tool = ToolSearchTool::new(Arc::new(bundled))
            .with_all_tools(Arc::new(bundled))
            .with_discovery(Arc::new(move |n| disc.lock().unwrap().push(n.to_string())));
        let out = tool
            .call(serde_json::json!({"query": "select:mcp__clock-mcp__clock_now"}), &ctx())
            .await
            .unwrap();
        let ToolOutput::Result { data, result_for_assistant } = &out[0] else {
            panic!();
        };
        let names: Vec<&str> = data["matches"].as_array().unwrap().iter().filter_map(|m| m["name"].as_str()).collect();
        assert_eq!(names, vec!["mcp__clock-mcp__clock_get_time"], "the tool that mentions \"now\"");
        assert_eq!(*discovered.lock().unwrap(), vec!["mcp__clock-mcp__clock_get_time".to_string()]);
        assert!(result_for_assistant.contains("No tool is named `mcp__clock-mcp__clock_now`"), "{result_for_assistant}");
        assert!(result_for_assistant.contains("All 2 tool(s) in `clock-mcp`"), "{result_for_assistant}");
        assert!(!result_for_assistant.contains("No registered tool"), "{result_for_assistant}");
    }

    #[test]
    fn closest_tools_keep_every_tool_that_shares_the_word() {
        // What models sent three times: `ssh_connect` for a server whose
        // connect tools all score alike.
        let pool = vec![
            stub("mcp__ssh-manager-mcp__ssh_check_connect", "Check a connection."),
            stub("mcp__ssh-manager-mcp__ssh_close_connect", "Close a connection."),
            stub("mcp__ssh-manager-mcp__ssh_execute_command", "Run a command."),
            stub("mcp__ssh-manager-mcp__ssh_list_connected", "List connections."),
            stub("mcp__ssh-manager-mcp__ssh_start_connect", "Open a connection."),
            stub("mcp__ssh-manager-mcp__ssh_start_connect_id", "Open a connection by id."),
        ];
        let c = closest_tools("mcp__ssh-manager-mcp__ssh_connect", &pool).unwrap();
        let near: Vec<&str> = c.near.iter().map(|t| t.name()).collect();
        assert!(near.contains(&"mcp__ssh-manager-mcp__ssh_start_connect_id"), "{near:?}");
        assert!(!near.contains(&"mcp__ssh-manager-mcp__ssh_execute_command"), "{near:?}");
        assert_eq!((c.label.as_str(), c.total), ("ssh-manager-mcp", 6));
    }

    #[tokio::test]
    async fn a_select_for_a_server_with_no_tools_says_so_and_loads_nothing() {
        let tool = ToolSearchTool::new(Arc::new(bundled)).with_all_tools(Arc::new(bundled));
        let out = tool
            .call(serde_json::json!({"query": "select:mcp__predict-mcp__predict_status"}), &ctx())
            .await
            .unwrap();
        let ToolOutput::Result { data, result_for_assistant } = &out[0] else {
            panic!();
        };
        assert_eq!(data["matches"].as_array().unwrap().len(), 0);
        assert!(result_for_assistant.contains("No registered tool for: mcp__predict-mcp__predict_status"));
        assert!(result_for_assistant.contains("Nothing from `predict-mcp` is registered"), "{result_for_assistant}");
    }

    #[tokio::test]
    async fn a_select_by_a_stale_name_confirms_the_bundled_tool() {
        let tool = ToolSearchTool::new(Arc::new(bundled)).with_all_tools(Arc::new(bundled));
        let out = tool
            .call(
                serde_json::json!({"query": "select:mcp__senclaw-browser__browser_search,mcp__browser__navigate"}),
                &ctx(),
            )
            .await
            .unwrap();
        let ToolOutput::Result { data, result_for_assistant } = &out[0] else {
            panic!();
        };
        let names: Vec<&str> = data["matches"].as_array().unwrap().iter().filter_map(|m| m["name"].as_str()).collect();
        assert_eq!(names, vec!["mcp__core__browser_search", "mcp__core__browser_navigate"]);
        assert!(result_for_assistant.starts_with("Loaded 2 tool(s)"), "{result_for_assistant}");
    }

    #[test]
    fn mentions_are_every_full_tool_name_once_in_order() {
        let text = "Run `ToolSearch { query: \"select:mcp__senclaw-browser__browser_search,mcp__space__note_create\" }`, \
                    then call mcp__space__note_create. Tools are listed as mcp__core__*; see mcp__browser__.";
        assert_eq!(
            mcp_mentions(text),
            vec!["mcp__senclaw-browser__browser_search", "mcp__space__note_create"]
        );
        assert!(mcp_mentions("tiếng Việt mcp__x__y… và mcp__a-b__c_d.").contains(&"mcp__a-b__c_d".to_string()));
    }

    /// A deferred tool with a name read at run time.
    struct NamedTool {
        name: String,
        desc: String,
    }

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            &self.desc
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn call(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
            Ok(vec![])
        }
        fn gen_tool_result_message(&self, _d: &Value, _i: &Value) -> ToolResultMessage {
            ToolResultMessage {
                title: String::new(),
                summary: String::new(),
                content: Value::Null,
            }
        }
        fn get_display_title(&self, _i: &Value) -> String {
            self.name.clone()
        }
        fn should_defer(&self) -> bool {
            true
        }
    }

    /// Replays recorded `ToolSearch` queries against a recorded tool catalog:
    /// `SENCLAW_TOOLSEARCH_REPLAY=<file.json>` with
    /// `{"tools": [{"name", "description"}], "searches": [{"query", "matched"}]}`
    /// (`matched` = how many tools the query loaded when it was made). Prints
    /// how many loaded nothing then and now; fails if one that loaded something
    /// then loads nothing now.
    #[tokio::test]
    #[ignore]
    async fn replay_recorded_searches() {
        let path = std::env::var("SENCLAW_TOOLSEARCH_REPLAY").expect("SENCLAW_TOOLSEARCH_REPLAY");
        let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let tools: Vec<Arc<dyn Tool>> = fixture["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                Arc::new(NamedTool {
                    name: t["name"].as_str().unwrap().to_string(),
                    desc: t["description"].as_str().unwrap_or("").to_string(),
                }) as Arc<dyn Tool>
            })
            .collect();
        let pool = tools.clone();
        let search = ToolSearchTool::new(Arc::new(move || pool.clone())).with_all_tools(Arc::new({
            let pool = tools.clone();
            move || pool.clone()
        }));
        let (mut total, mut empty_then, mut empty_now, mut lost) = (0, 0, 0, Vec::new());
        let (mut select_then, mut select_now, mut keyword_then, mut keyword_now) = (0, 0, 0, 0);
        let (mut by_name, mut by_closest) = (0, 0);
        for s in fixture["searches"].as_array().unwrap() {
            let query = s["query"].as_str().unwrap();
            let before = s["matched"].as_u64().unwrap();
            let out = search
                .call(serde_json::json!({"query": query, "max_results": 5}), &ctx())
                .await
                .unwrap();
            let ToolOutput::Result { data, result_for_assistant } = &out[0] else {
                panic!();
            };
            let after = data["matches"].as_array().unwrap().len();
            let is_select = query.starts_with(SELECT_PREFIX);
            if is_select && before == 0 && after > 0 {
                if result_for_assistant.starts_with("Loaded") {
                    by_name += 1;
                } else {
                    by_closest += 1;
                }
            }
            total += 1;
            if before == 0 {
                empty_then += 1;
                *(if is_select { &mut select_then } else { &mut keyword_then }) += 1;
            }
            if after == 0 {
                empty_now += 1;
                *(if is_select { &mut select_now } else { &mut keyword_now }) += 1;
                if before > 0 {
                    lost.push(query.to_string());
                }
            }
        }
        println!(
            "{total} searches: loaded nothing then {empty_then} (select {select_then}, keyword {keyword_then}), now {empty_now} (select {select_now}, keyword {keyword_now}); select recovered by name {by_name}, by closest tools {by_closest}"
        );
        assert!(lost.is_empty(), "loaded something then, nothing now: {lost:?}");
    }

    #[test]
    fn always_load_is_true_so_tool_search_never_deferred() {
        let resolver: DeferredToolsFn = Arc::new(Vec::new);
        let t = ToolSearchTool::new(resolver);
        assert!(t.always_load());
        // Sanity: should_defer default is false; ToolSearch never opts in.
        assert!(!t.should_defer());
    }
}

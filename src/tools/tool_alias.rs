//! MCP tool aliases (Plugins → Alias) — rename or override tools by config.
//!
//! A process-wide registry maps `alias name → target tool name`, loaded from
//! the `mcp_tool_aliases` table (see [`crate::db::tool_aliases`]) at boot and
//! refreshed after every mutation through the REST API or a Space App import.
//!
//! Two behaviours, one mapping:
//!   * **override** — the alias equals a registered tool name. Stage 0 of
//!     [`crate::tools::tool_search::resolve_tool_by_name`] rewrites the call
//!     before exact matching, so the original implementation is shadowed and
//!     the target executes instead. The roster is unchanged.
//!   * **rename** — the alias is a new name. [`apply_alias_names`] swaps the
//!     target tool for an [`AliasedTool`] wrapper in the roster funnels, so
//!     the LLM sees (and calls) the alias. The original name keeps resolving
//!     through [`crate::zen_core::Tool::renamed_from`], so old transcripts,
//!     skills, and hardcoded references don't break.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::Result;
use serde_json::Value;

use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolPermissionInfo, ToolResultMessage};

/// One enabled alias entry: the tool that should actually run, plus an
/// optional human description shown in place of the target's.
#[derive(Debug, Clone)]
pub struct AliasEntry {
    pub target: String,
    pub description: Option<String>,
}

static REGISTRY: OnceLock<RwLock<HashMap<String, AliasEntry>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<String, AliasEntry>> {
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Replace the process-wide alias map (enabled entries only).
pub fn set_alias_map(map: HashMap<String, AliasEntry>) {
    *registry().write().expect("alias registry poisoned") = map;
}

/// Reload the registry from the DB. Call after any alias mutation.
pub fn reload_from_db(db: &crate::db::Db) {
    match db.enabled_tool_alias_map() {
        Ok(rows) => {
            let map = rows
                .into_iter()
                .map(|(alias, (target, description))| (alias, AliasEntry { target, description }))
                .collect::<HashMap<_, _>>();
            let n = map.len();
            set_alias_map(map);
            tracing::debug!("tool alias registry reloaded: {n} enabled aliases");
        }
        Err(e) => tracing::warn!("tool alias registry reload failed: {e:#}"),
    }
}

/// Resolve `name` through the enabled alias map, following chains
/// (`a → b → c`) with a cycle guard. `None` when `name` is not an alias.
pub fn resolve_alias(name: &str) -> Option<String> {
    let map = registry().read().expect("alias registry poisoned");
    if map.is_empty() {
        return None;
    }
    resolve_alias_in(&map, name)
}

fn resolve_alias_in(map: &HashMap<String, AliasEntry>, name: &str) -> Option<String> {
    let mut current = map.get(name)?.target.clone();
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(name.to_string());
    while seen.insert(current.clone()) {
        match map.get(&current) {
            // Hop cap keeps a pathological chain from spinning; 8 is far
            // beyond any sane configuration.
            Some(next) if seen.len() <= 8 => current = next.target.clone(),
            _ => break,
        }
    }
    Some(current)
}

// ============================================================================
// Roster decoration (rename)
// ============================================================================

/// A tool presented under a configured alias. Everything delegates to the
/// wrapped tool except the name (and optionally the description).
pub struct AliasedTool {
    alias: String,
    description: Option<String>,
    inner: Arc<dyn Tool>,
}

impl AliasedTool {
    pub fn new(alias: String, description: Option<String>, inner: Arc<dyn Tool>) -> Self {
        Self {
            alias,
            description,
            inner,
        }
    }
}

#[async_trait::async_trait]
impl Tool for AliasedTool {
    fn name(&self) -> &str {
        &self.alias
    }
    fn description(&self) -> &str {
        self.description
            .as_deref()
            .unwrap_or_else(|| self.inner.description())
    }
    fn input_schema(&self) -> Value {
        self.inner.input_schema()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    /// The alias is a display name, never a permission category: without this
    /// an aliased `Edit` fell through every branch of the checker (not
    /// file-edit, not Bash, not Skill, not `mcp__`) and landed on "other
    /// non-readonly tool — default allow", writing files with no prompt at all.
    fn permission_name(&self) -> &str {
        self.inner.permission_name()
    }
    /// Forwarded like everything else: without this an aliased `Edit`/`Write`
    /// inherits the empty default, `run_tools` skips path resolution for it,
    /// and the approval card shows the user the raw path the model typed
    /// while the tool's own second layer writes somewhere else.
    fn path_fields(&self) -> &'static [&'static str] {
        self.inner.path_fields()
    }
    async fn validate_input(
        &self,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        self.inner.validate_input(input, ctx).await
    }
    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        self.inner.call(input, ctx).await
    }
    fn gen_tool_result_message(&self, data: &Value, input: &Value) -> ToolResultMessage {
        self.inner.gen_tool_result_message(data, input)
    }
    fn get_display_title(&self, input: &Value) -> String {
        self.inner.get_display_title(input)
    }
    fn gen_tool_permission(&self, input: &Value) -> Option<ToolPermissionInfo> {
        self.inner.gen_tool_permission(input)
    }
    fn search_hint(&self) -> String {
        self.inner.search_hint()
    }
    fn should_defer(&self) -> bool {
        self.inner.should_defer()
    }
    fn always_load(&self) -> bool {
        self.inner.always_load()
    }
    fn aliases(&self) -> &[&str] {
        self.inner.aliases()
    }
    fn renamed_from(&self) -> Option<&str> {
        Some(self.inner.name())
    }
}

/// Apply rename aliases to a tool roster: each enabled alias whose name does
/// NOT collide with a registered tool replaces its target tool with an
/// [`AliasedTool`] carrying the alias name. Colliding aliases are overrides —
/// they redirect at dispatch time and leave the roster untouched.
///
/// Idempotent and deterministic (aliases applied in sorted order) so repeated
/// application inside the per-turn roster funnels keeps the tool list stable
/// for prompt caching.
pub fn apply_alias_names(mut tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
    let snapshot = {
        let map = registry().read().expect("alias registry poisoned");
        if map.is_empty() {
            return tools;
        }
        map.clone()
    };
    let existing: HashSet<String> = tools.iter().map(|t| t.name().to_string()).collect();
    let mut ordered: Vec<(&String, &AliasEntry)> = snapshot.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));
    for (alias, entry) in ordered {
        if existing.contains(alias) {
            continue; // override — handled at dispatch, roster unchanged
        }
        let final_target =
            resolve_alias_in(&snapshot, alias).unwrap_or_else(|| entry.target.clone());
        let Some(target_tool) =
            crate::tools::tool_search::resolve_tool_ignoring_aliases(&final_target, &tools)
        else {
            continue; // target not registered (app off / bad name) — skip
        };
        let target_name = target_tool.name().to_string();
        if target_name == *alias {
            continue;
        }
        if let Some(idx) = tools.iter().position(|t| t.name() == target_name) {
            // One rename per tool: a tool already renamed by an earlier alias
            // keeps that name; later aliases still work at dispatch time.
            if tools[idx].renamed_from().is_some() {
                continue;
            }
            tools[idx] = Arc::new(AliasedTool::new(
                alias.clone(),
                entry.description.clone(),
                Arc::clone(&tools[idx]),
            ));
        }
    }
    tools
}

// ============================================================================
// Space App manifest import
// ============================================================================

/// An alias declared by a Space App in `senclaw-manifest.json` → `mcp.toolAliases`.
#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredAlias {
    pub alias: String,
    pub target: String,
    pub description: Option<String>,
}

/// Parse `mcp.toolAliases` from a Space App manifest `mcp` block.
///
/// Entry shape: `{ "alias": "...", "target": "...", "description": "..." }`
/// (`"tool"` is accepted as a synonym for `"target"`). A bare target name is
/// expanded to `mcp__<server_name>__<target>`. Aliases MUST be full `mcp__*`
/// names — a bare alias could shadow a builtin tool (Bash, Read, Write, ...),
/// which would let a manifest spoof core tools.
pub fn parse_declared_aliases(server_name: &str, mcp: &Value) -> Vec<DeclaredAlias> {
    let Some(items) = mcp.get("toolAliases").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let alias = item
            .get("alias")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let target_raw = item
            .get("target")
            .or_else(|| item.get("tool"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if alias.is_empty() || target_raw.is_empty() {
            tracing::warn!("toolAliases entry missing alias/target — skipped: {item}");
            continue;
        }
        if alias.contains(char::is_whitespace) || target_raw.contains(char::is_whitespace) {
            tracing::warn!("toolAliases entry contains whitespace — skipped: {item}");
            continue;
        }
        if !alias.starts_with("mcp__") {
            tracing::warn!(
                "toolAliases alias '{alias}' rejected: app aliases must be full mcp__* names \
                 (bare names could shadow builtin tools)"
            );
            continue;
        }
        let target = if target_raw.starts_with("mcp__") {
            target_raw.to_string()
        } else {
            format!("mcp__{server_name}__{target_raw}")
        };
        if alias == target {
            continue;
        }
        out.push(DeclaredAlias {
            alias: alias.to_string(),
            target,
            description: item
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(target: &str) -> AliasEntry {
        AliasEntry {
            target: target.to_string(),
            description: None,
        }
    }

    /// Minimal Tool stub for resolution tests.
    struct Stub {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl Tool for Stub {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "stub"
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
            self.name.to_string()
        }
    }

    /// Full-path test through the process-global registry: override shadows an
    /// existing tool, a rename decorates the roster, the original name keeps
    /// resolving via `renamed_from`, and a missing target degrades gracefully.
    /// Uses `zz_`-prefixed names unique to this test so parallel tests that
    /// share the global registry can't be affected.
    #[test]
    fn global_registry_rename_and_override_resolution() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(Stub {
                name: "zz_alias_orig_tool",
            }),
            Arc::new(Stub {
                name: "zz_alias_target_tool",
            }),
        ];
        let mut map = HashMap::new();
        map.insert(
            "mcp__zzalias__renamed".to_string(),
            entry("zz_alias_target_tool"),
        );
        map.insert(
            "zz_alias_orig_tool".to_string(),
            entry("zz_alias_target_tool"),
        );
        set_alias_map(map);

        // Override: calling the existing name executes the target instead.
        let t = crate::tools::tool_search::resolve_tool_by_name("zz_alias_orig_tool", &tools)
            .expect("override resolves");
        assert_eq!(t.name(), "zz_alias_target_tool");

        // Rename alias resolves even without roster decoration.
        let t = crate::tools::tool_search::resolve_tool_by_name("mcp__zzalias__renamed", &tools)
            .expect("rename alias resolves");
        assert_eq!(t.name(), "zz_alias_target_tool");

        // Roster decoration: the target shows under the alias name; the
        // override leaves the roster untouched.
        let decorated = apply_alias_names(tools.clone());
        let names: Vec<&str> = decorated.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"mcp__zzalias__renamed"));
        assert!(names.contains(&"zz_alias_orig_tool"));
        assert!(!names.contains(&"zz_alias_target_tool"));

        // Idempotent — funnels may decorate an already-decorated list.
        let twice = apply_alias_names(decorated.clone());
        let names2: Vec<&str> = twice.iter().map(|t| t.name()).collect();
        assert_eq!(names, names2);

        // The original registered name still resolves (renamed_from stage).
        let t =
            crate::tools::tool_search::resolve_tool_by_name("zz_alias_target_tool", &decorated)
                .expect("original name resolves after rename");
        assert_eq!(t.name(), "mcp__zzalias__renamed");
        assert_eq!(t.renamed_from(), Some("zz_alias_target_tool"));

        // A missing target falls back to the original tool — an alias must
        // never brick a working tool.
        let mut bad = HashMap::new();
        bad.insert("zz_alias_orig_tool".to_string(), entry("zz_alias_gone"));
        set_alias_map(bad);
        let t = crate::tools::tool_search::resolve_tool_by_name("zz_alias_orig_tool", &tools)
            .expect("fallback to original");
        assert_eq!(t.name(), "zz_alias_orig_tool");

        set_alias_map(HashMap::new());
    }

    #[test]
    fn resolve_follows_chains_and_survives_cycles() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), entry("b"));
        map.insert("b".to_string(), entry("c"));
        assert_eq!(resolve_alias_in(&map, "a").as_deref(), Some("c"));
        assert_eq!(resolve_alias_in(&map, "b").as_deref(), Some("c"));
        assert_eq!(resolve_alias_in(&map, "c"), None);

        // Cycle a → b → a degrades to the original name (caller falls back).
        let mut cyc = HashMap::new();
        cyc.insert("a".to_string(), entry("b"));
        cyc.insert("b".to_string(), entry("a"));
        assert_eq!(resolve_alias_in(&cyc, "a").as_deref(), Some("a"));
    }

    #[test]
    fn parse_declared_aliases_expands_and_validates() {
        let mcp = serde_json::json!({
            "name": "demo-mcp",
            "toolAliases": [
                { "alias": "mcp__demo__short", "tool": "demo_long_tool", "description": "d" },
                { "alias": "mcp__senclaw-browser__browser_navigate", "target": "mcp__demo-mcp__demo_nav" },
                { "alias": "Bash", "target": "demo_evil" },
                { "alias": "mcp__demo__self", "target": "mcp__demo__self" },
                { "alias": "", "target": "x" },
                { "alias": "mcp__demo__ws", "target": "has space" }
            ]
        });
        let got = parse_declared_aliases("demo-mcp", &mcp);
        assert_eq!(
            got,
            vec![
                DeclaredAlias {
                    alias: "mcp__demo__short".into(),
                    target: "mcp__demo-mcp__demo_long_tool".into(),
                    description: Some("d".into()),
                },
                DeclaredAlias {
                    alias: "mcp__senclaw-browser__browser_navigate".into(),
                    target: "mcp__demo-mcp__demo_nav".into(),
                    description: None,
                },
            ]
        );
        assert!(parse_declared_aliases("demo-mcp", &serde_json::json!({})).is_empty());
    }
}

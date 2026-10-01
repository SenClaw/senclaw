//! ZenEngine — per-instance agent runtime orchestrator.
//!
//! Owns the [`EventBus`], [`StateManager`], tool registry, and config.
//! Implements [`ZenCore`] so SenClaw's [`AgentPool`] can drive it without
//! knowing about internal engine details.
//!
//! Port of TS `ZenEngine` from sema-core.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::config_manager;
use super::hooks::{
    self as zen_hooks, ExecuteHooksOptions, HookEvent, HookInput, HookInputBase, HookManager,
    SessionInput, StopInput, UserPromptSubmitInput,
};
use super::*;
use crate::gateway::group_manager::load_llm_configs;
use crate::mcp::SharedMcpRegistry;
use crate::skills::SkillRegistry;
use crate::tools::tool_search::normalize_mcp_tool_name;
use crate::tools::{SkillTool, TaskTool, TodoWriteTool, ToolSearchTool};
use events::ResponseRegistry;
use permissions::PermissionManager;

/// Whether a tool counts as "discovered", tolerant of MCP naming schemes.
///
/// The discovered set may hold either the full registered name
/// (`mcp__senclaw-browser__browser_search`, inserted by ToolSearch /
/// `apply_skill_activation`) or the stripped bridge name
/// (`mcp__browser__search`, inserted by the `agent-browser` load hook).
/// Normalizing the queried tool's name collapses both onto the same key so a
/// tool pre-discovered under either form un-defers correctly.
fn discovered_has(set: &DiscoveredTools, name: &str) -> bool {
    set.contains(name) || set.contains(&normalize_mcp_tool_name(name))
}

/// Whether `text` names `ident` as a whole identifier — not inside a longer
/// one (`get` in `budget`, `space_list` in `space_list_all`).
fn mentions_identifier(text: &str, ident: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(ident).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + ident.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

/// Most deferred tools a session keeps active at once through `ToolSearch` or
/// a skill's pre-discovery. Every active tool's schema is sent on every call,
/// and the set only ever grew: one session reached 93 tools, and the larger the
/// list the sooner a request hits a provider's tool or context limit. 24 keeps
/// a turn near 55 tools on top of the ~30 always-loaded ones.
pub(crate) const MAX_DISCOVERED_TOOLS: usize = 24;

/// Deferred tools this session made callable.
///
/// Two kinds: `pinned` — named by the agent's own configuration (`use_tools`,
/// DAG mode's dispatch tools), always kept — and `recent`, what `ToolSearch`
/// and skill activation loaded, capped at [`MAX_DISCOVERED_TOOLS`] with the
/// least recently (re)discovered dropped first. A dropped tool is still one
/// `ToolSearch` away; the deferred-tools reminder keeps listing it.
#[derive(Debug, Default, Clone)]
pub(crate) struct DiscoveredTools {
    pinned: std::collections::HashSet<String>,
    recent: std::collections::VecDeque<String>,
}

impl DiscoveredTools {
    /// Mark `name` discovered (most recent first in line to stay). Returns
    /// whether it was not active before, like `HashSet::insert`.
    pub(crate) fn insert(&mut self, name: String) -> bool {
        if self.pinned.contains(&name) {
            return false;
        }
        let was_new = match self.recent.iter().position(|n| *n == name) {
            Some(i) => {
                self.recent.remove(i);
                false
            }
            None => true,
        };
        self.recent.push_back(name);
        while self.recent.len() > MAX_DISCOVERED_TOOLS {
            if let Some(dropped) = self.recent.pop_front() {
                tracing::info!("[ToolSearch] tool budget: deactivated least recently discovered {dropped}");
            }
        }
        was_new
    }

    /// Keep `name` active for the whole session, outside the cap.
    pub(crate) fn pin(&mut self, name: String) {
        self.recent.retain(|n| *n != name);
        self.pinned.insert(name);
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.pinned.contains(name) || self.recent.iter().any(|n| n == name)
    }

    pub(crate) fn clear(&mut self) {
        self.pinned.clear();
        self.recent.clear();
    }
}

/// Per-instance agent execution engine.
///
/// Each chat JID gets one engine. The engine is driven by [`ZenCore`] method
/// calls from [`AgentPool`] and emits events back through the [`EventBus`].
pub struct ZenEngine {
    pub instance_id: String,
    pub event_bus: EventBus,
    state: Arc<Mutex<StateManager>>,
    response_registry: Arc<ResponseRegistry>,
    permission_manager: Arc<PermissionManager>,
    handlers: RwLock<ZenCoreHandlers>,

    // HTTP client for LLM calls
    http_client: Client,

    // Config
    pub(crate) options: RwLock<ZenCoreOptions>,

    // Tool registry
    builtin_tools: RwLock<Vec<Arc<dyn Tool>>>,

    // Skill registry (shared with Skill tool)
    skill_registry: Arc<SkillRegistry>,

    /// This turn's pre-skill decision from the router
    /// (`crate::decision::skill_route`), taken once by the next `start_query`.
    /// `Some(None)` = routed, nothing to suggest; `None` = not routed, so the
    /// legacy keyword matcher decides.
    skill_route: Mutex<Option<Option<crate::skills::matching::SkillRoute>>>,

    // MCP subprocess registry
    mcp_registry: SharedMcpRegistry,

    // External MCP server manager (bridges user-configured MCP tools)
    pub mcp_manager: Option<Arc<crate::mcp::manager::McpManager>>,

    // Session helpers
    session_id: RwLock<Option<String>>,

    // Hook system
    pub hook_manager: Arc<HookManager>,

    // Workbench artifact service (artifact publishing + reverse ops)
    pub workbench_service: Arc<crate::zen_core::workbench::WorkbenchService>,

    /// Tool names that became available via `ToolSearch` during this session.
    /// `tools_for_main_agent` includes these even when `should_defer() == true`,
    /// so the model can actually call what ToolSearch promised. Reset on
    /// `dispose()` / new session.
    pub(crate) discovered_tools: Arc<Mutex<DiscoveredTools>>,

    /// Weak self-reference set during construction. Lets `&self` methods hand
    /// out closures that re-fetch live engine state without holding a strong
    /// ref (which would prevent drop). Mirror of `AgentPool::self_weak`.
    self_weak: Mutex<std::sync::Weak<Self>>,

    /// After hydrating a persisted trajectory (daemon restart / stop-without-clear),
    /// the next user turn re-injects first-turn identity (profile, date, project
    /// docs) even though `messages` is non-empty — those blocks were only on the
    /// original first user message and are often dropped by compaction.
    identity_refresh_pending: Arc<AtomicBool>,
}

impl ZenEngine {
    pub fn new(
        options: ZenCoreOptions,
        mcp_manager: Option<Arc<crate::mcp::manager::McpManager>>,
    ) -> Arc<Self> {
        let instance_id = options.instance_id.clone();
        let event_bus = EventBus::new();
        let response_registry = Arc::new(ResponseRegistry::new());
        let permission_manager = Arc::new(PermissionManager::new(
            event_bus.clone(),
            response_registry.clone(),
        ));
        // Wire the option skip flags in — without this, unattended sessions
        // (isolated runner / workflow agent steps) emit permission requests
        // that nobody can answer and hang until their timeout.
        permission_manager.update_skip_flags(
            options.skip_file_edit_permission,
            options.skip_bash_exec_permission,
            options.skip_skill_permission,
            options.skip_mcp_tool_permission,
        );

        // A **read** timeout, not a total one: this client only ever carries LLM
        // requests, and a total deadline is a ceiling on how long a generation
        // may legitimately take — which a local engine exceeds honestly at
        // 8192 tokens. `query_llm::post_authed` re-applies a total deadline for
        // remote providers, where a hung endpoint is the likelier failure.
        let http_client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .read_timeout(query_llm::STREAM_STALL_TIMEOUT)
            .build()
            .expect("Failed to create HTTP client");

        let state = Arc::new(Mutex::new(StateManager::new()));
        let skill_registry = Arc::new(SkillRegistry::default());

        let workbench_service = Arc::new(crate::zen_core::workbench::WorkbenchService::new(
            event_bus.clone(),
            instance_id.clone(),
            options.working_dir.clone(),
        ));

        let engine = Arc::new(Self {
            instance_id,
            event_bus,
            state: state.clone(),
            response_registry,
            permission_manager,
            handlers: RwLock::new(ZenCoreHandlers::default()),
            http_client,
            options: RwLock::new(options),
            builtin_tools: RwLock::new(Vec::new()),
            skill_registry: skill_registry.clone(),
            skill_route: Mutex::new(None),
            mcp_registry: SharedMcpRegistry::new(),
            mcp_manager,
            session_id: RwLock::new(None),
            hook_manager: Arc::new(HookManager::empty()),
            workbench_service: workbench_service.clone(),
            discovered_tools: Arc::new(Mutex::new(DiscoveredTools::default())),
            self_weak: Mutex::new(std::sync::Weak::new()),
            identity_refresh_pending: Arc::new(AtomicBool::new(false)),
        });
        *engine.self_weak.lock().unwrap() = Arc::downgrade(&engine);

        // The hook machinery below (matching, command/prompt executors,
        // PreToolUse gating) was fully built but never handed a config: every
        // engine started with `HookManager::empty()` and nothing ever called
        // the loader, so `~/.senclaw/hooks.json` sat inert on disk. Load it
        // here, once per engine, so hooks actually fire. Costs one stat when
        // no hooks file exists.
        engine.reload_hooks();

        // Register engine-dependent tools
        engine.register_tool(Arc::new(TodoWriteTool::new(state)));
        // When ANY skill is loaded via the `Skill` tool, run the generic skill
        // activation: pre-discover (un-defer) the deferred MCP tools the skill's
        // instructions reference, and inject its configured env. This mirrors the
        // `#name` / `/name` force-load path (`force_skill_reminder`). Previously
        // this callback was hardcoded to `agent-browser` only, so every Space-App
        // skill (ssh-connect, email-reporting, google-workspace, …) loaded its
        // instructions but left its MCP tools deferred — the model then called
        // e.g. `ssh_list_hosts` and hit "No such tool available".
        let engine_for_skill = Arc::downgrade(&engine);
        let on_skill_load: crate::tools::skill::OnSkillLoadFn = Arc::new(move |skill_name| {
            if let Some(e) = engine_for_skill.upgrade() {
                if let Some(skill) = e.skill_registry.find(skill_name) {
                    e.apply_skill_activation(&skill);
                }
            }
        });
        engine.register_tool(Arc::new(
            SkillTool::new(skill_registry).with_on_load(on_skill_load),
        ));
        // LaunchUI — surfaces deliverables in the WebUI workbench panel.
        engine.register_tool(Arc::new(crate::tools::LaunchUITool::new(workbench_service)));

        // Register static tools (no engine deps)
        engine.register_tools(crate::tools::all_tools());

        // Register ToolSearch — discovery mechanism for deferred tools. Uses
        // Weak<Self> so the resolver re-fetches live `deferred_tools()` on
        // each call (use_tools / Plan / cowork filters apply).
        let engine_for_search = Arc::downgrade(&engine);
        let deferred_resolver: crate::tools::DeferredToolsFn = Arc::new(move || {
            engine_for_search
                .upgrade()
                .map(|e| e.deferred_tools())
                .unwrap_or_default()
        });
        let engine_for_discovery = Arc::downgrade(&engine);
        let register_discovered: crate::tools::tool_search::RegisterDiscoveredFn =
            Arc::new(move |name: &str| {
                if let Some(e) = engine_for_discovery.upgrade() {
                    e.discovered_tools.lock().unwrap().insert(name.to_string());
                    tracing::info!("[ToolSearch] discovered tool: {name}");
                }
            });
        // Skill resolver: lets ToolSearch surface installed SKILLS by keyword
        // (e.g. `ToolSearch("ssh")` → `ssh-connect`, `ssh-guide`, …), not just
        // deferred tools. Skills are still invoked via the `Skill` tool. Excludes
        // `use: always` skills (already fully injected) and model-hidden ones.
        let engine_for_skills = Arc::downgrade(&engine);
        let skills_resolver: crate::tools::tool_search::SkillsFn = Arc::new(move || {
            let Some(e) = engine_for_skills.upgrade() else {
                return Vec::new();
            };
            e.skill_registry
                .names()
                .iter()
                .filter_map(|n| e.skill_registry.find(n))
                .filter(|s| {
                    !s.metadata.disable_model_invocation
                        && s.metadata.use_mode != crate::skills::SkillUseMode::Always
                })
                .map(|s| crate::tools::tool_search::SkillSearchRow {
                    name: s.metadata.name.clone(),
                    description: s.metadata.description.clone(),
                    when_to_use: s.metadata.when_to_use.clone(),
                    triggers: s.metadata.triggers.clone(),
                })
                .collect()
        });
        // All-tools resolver: lets ToolSearch's `select:` path resolve names
        // against the full available set (active + deferred), so selecting an
        // already-loaded tool like `Skill` confirms it rather than dead-ending
        // on "0 matches".
        let engine_for_all = Arc::downgrade(&engine);
        let all_tools_resolver: crate::tools::tool_search::AllToolsFn = Arc::new(move || {
            engine_for_all
                .upgrade()
                .map(|e| e.available_tools())
                .unwrap_or_default()
        });
        engine.register_tool(Arc::new(
            ToolSearchTool::new(deferred_resolver)
                .with_discovery(register_discovered)
                .with_skills(skills_resolver)
                .with_all_tools(all_tools_resolver),
        ));

        // EnterPlanMode — flip the engine's `agent_mode` to Plan. Mirror of
        // `ExitPlanMode` (which requests approval). Both are `always_load`
        // builtins so the model doesn't need ToolSearch to find them.
        let engine_for_plan = Arc::downgrade(&engine);
        engine.register_tool(Arc::new(crate::tools::EnterPlanModeTool::for_engine(
            engine_for_plan,
        )));

        // Register TaskTool last so it knows about all other tools.
        // Pass resolver closures (vs snapshots) so spawned subagents inherit
        // `use_tools` / Plan-mode / cowork filters AND the engine's live model
        // selection (per-group LLM override) as they evolve at runtime.
        let engine_for_resolver = Arc::downgrade(&engine);
        let tools_resolver: crate::tools::task::ToolResolver = Arc::new(move || {
            engine_for_resolver
                .upgrade()
                .map(|e| e.tools_for_main_agent())
                .unwrap_or_default()
        });
        let engine_for_profile = Arc::downgrade(&engine);
        let profile_resolver: crate::tools::task::ProfileResolver = Arc::new(move || {
            engine_for_profile
                .upgrade()
                .map(|e| e.resolve_model_profile())
                .unwrap_or_else(|| ZenEngine::resolve_model_profile_with(None))
        });
        engine.register_tool(Arc::new(TaskTool::new(
            engine.http_client.clone(),
            engine.event_bus.clone(),
            engine.state.clone(),
            engine.permission_manager.clone(),
            crate::tools::task::default_agent_configs(),
            engine.options.read().unwrap().working_dir.clone(),
            engine.options.read().unwrap().agent_data_dir.clone(),
            tools_resolver,
            profile_resolver,
        )));

        // Register custom memory directory with MemoryManager if provided
        if let Some(ref custom_memory_dir) = engine.options.read().unwrap().custom_memory_dir {
            if let Some(memory_mgr) = crate::memory::manager::try_get_instance() {
                let opts = engine.options.read().unwrap();
                let folder_key = opts
                    .memory_folder_override
                    .as_deref()
                    .unwrap_or(opts.agent_data_dir.as_str());
                let instance_id_for_log = opts.instance_id.clone();
                memory_mgr.register_custom_memory_dir(
                    folder_key,
                    std::path::PathBuf::from(custom_memory_dir),
                );
                tracing::info!(
                    "[ZenEngine] Registered custom memory dir for instance '{}' (folder={folder_key}): {}",
                    instance_id_for_log,
                    custom_memory_dir
                );
            }
        }

        engine
    }

    // ============================================================
    // Tool registry
    // ============================================================

    /// Load hooks from disk into this engine's [`HookManager`].
    ///
    /// Three sources, in the loader's own trust order:
    ///
    /// | source | trust |
    /// |---|---|
    /// | `~/.senclaw/hooks.json` | user-authored — may use `type: command` |
    /// | `<workspace>/.senclaw/hooks.json` | user-authored |
    /// | `<kits_dir>/hooks/*.json` (installed kits) | third-party — shell is refused unless `SENCLAW_ALLOW_MARKETPLACE_COMMAND_HOOKS=true` |
    ///
    /// Kit files ride in the `extra_files` slot precisely so they inherit that
    /// last rule: a bundle a user installed with one tap must not be able to
    /// register `sh -c` at daemon privilege.
    ///
    /// Safe to call again after installing or removing a kit. No hooks
    /// anywhere leaves the manager empty, which is what it was before.
    pub fn reload_hooks(&self) {
        let config = crate::config::Config::from_env();
        // `hooks_path` is `<senclaw home>/hooks.json`; the loader wants the
        // directory and appends the filename itself.
        let Some(global_dir) = config.paths.hooks_path.parent().map(|p| p.to_path_buf()) else {
            return;
        };
        let workspace = self.options.read().unwrap().working_dir.clone();
        let workspace_path = std::path::PathBuf::from(&workspace);
        let kit_files = crate::kits::kit_hook_files(&config.paths.kits_dir);

        let loaded = crate::agent::load_zen_hook_config(
            &global_dir,
            if workspace.is_empty() {
                None
            } else {
                Some(workspace_path.as_path())
            },
            if kit_files.is_empty() {
                None
            } else {
                Some(&kit_files)
            },
            crate::agent::MarketplaceHookPolicy::from_config(&config),
        );

        match loaded {
            Some(cfg) => {
                let events = cfg.hooks.len();
                self.hook_manager.update_config(cfg);
                tracing::info!("[hooks] loaded hooks for {events} event(s)");
            }
            // Not an error: most installs have no hooks at all.
            None => self
                .hook_manager
                .update_config(crate::zen_core::hooks::HookConfig::default()),
        }
    }

    pub fn register_tool(&self, tool: Arc<dyn Tool>) {
        self.builtin_tools.write().unwrap().push(tool);
    }

    pub fn register_tools(&self, tools: Vec<Arc<dyn Tool>>) {
        self.builtin_tools.write().unwrap().extend(tools);
    }

    /// Refresh external MCP bridge tools in the tool roster.
    /// Removes previously-registered `mcp__` tools and re-fetches from the
    /// McpManager.
    pub async fn refresh_mcp_tools(self: &Arc<Self>) {
        if let Some(ref mgr) = self.mcp_manager {
            let bridge_tools = crate::mcp::bridge::McpBridgeTool::from_manager(mgr).await;
            let mut tools = self.builtin_tools.write().unwrap();
            // Remove old MCP bridge tools
            tools.retain(|t| !t.name().starts_with("mcp__"));
            // Add refreshed ones
            tools.extend(bridge_tools);
        }
    }

    /// Resolve the tool list for the **main agent** turn. This is what gets
    /// serialized into the LLM API `tools` field every turn.
    ///
    /// Filter layers (applied in order — token-saving funnel):
    ///   1. **Registry**: all `builtin_tools` registered on the engine.
    ///   2. **`use_tools` whitelist**: empty = no restriction; otherwise keep
    ///      only names that appear. Mirrors sema-core `getAvailableBuiltinTools`.
    ///   3. **Plan-mode filter**: drops `TodoWrite` (Plan-mode policy).
    ///   4. **Cowork-mode filter**: drops interactive ask-tools for synthetic
    ///      `cowork:*` instance ids (no UI subscriber to answer questions).
    ///   5. **Defer filter** (claude-code pattern): drops tools whose
    ///      `should_defer() == true && !always_load()`. The LLM discovers
    ///      these via `ToolSearch`. Cuts ~80% of tool tokens for MCP-heavy
    ///      workloads.
    ///   6. **Stable sort**: alphabetical-by-name so the tool list is
    ///      byte-identical turn-over-turn — preserves Anthropic prompt cache
    ///      hits.
    ///
    /// This method does NOT apply the subagent exclusion list — call
    /// [`Self::tools_for_subagent`] for that path.
    pub fn tools_for_main_agent(&self) -> Vec<Arc<dyn Tool>> {
        let opts = self.options.read().unwrap();
        let use_tools = &opts.use_tools;
        let is_plan = opts.agent_mode == AgentMode::Plan;
        let is_dag = opts.agent_mode == AgentMode::Dag;
        // Rename aliases (Plugins → Alias) applied at the funnel so a toggle
        // takes effect on the next turn without re-registering tools.
        // Overrides don't touch the roster — they redirect at dispatch time.
        let tools =
            crate::tools::tool_alias::apply_alias_names(self.builtin_tools.read().unwrap().clone());

        let mut filtered: Vec<Arc<dyn Tool>> = if use_tools.is_empty() {
            tools.clone()
        } else {
            // Resolve each whitelist entry against the live tool list so a bare or
            // alternate MCP name (`ssh_list_hosts`, the same form skill docs use)
            // still matches the registered full name
            // (`mcp__ssh-manager-mcp__ssh_list_hosts`). A naive `t.name()` exact
            // match silently drops every MCP tool an admin whitelisted by short
            // name — the classic `allowed_tools` trap.
            let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();
            for entry in use_tools.iter() {
                if let Some(t) =
                    crate::tools::tool_search::resolve_tool_by_name(entry, tools.as_slice())
                {
                    keep.insert(t.name().to_string());
                }
            }
            tools
                .iter()
                // `always_load()` tools (e.g. `ToolSearch`) bypass the whitelist
                // entirely: stripping the discovery tool would strand the agent
                // with no way to load any deferred tool it still needs.
                .filter(|t| t.always_load() || keep.contains(t.name()))
                .cloned()
                .collect()
        };

        // Plan mode is read-only: physically strip every mutating tool so
        // the agent CANNOT edit files, run shell commands, or write todos —
        // the system prompt asks nicely, this enforces. The only non-read-only
        // tool kept is `ExitPlanMode`, which is how the agent requests approval
        // to leave plan mode and begin executing. This closes the gap where an
        // aggressive model (or prompt injection) ignores the prompt-level
        // constraint and calls Edit/Write/Bash anyway.
        if is_plan {
            // Tools allowed in plan mode despite `is_read_only() == false`:
            // they're research/escape tools with no destructive local effect.
            // WebFetch/WebSearch fetch external info (the "research" in
            // "read-only research"); ExitPlanMode is the approval escape hatch.
            //
            // Skill-discovered tools (via `apply_skill_activation`) are also
            // exempt: the user explicitly triggered a skill (e.g. calendar),
            // and the skill's pre-discovered MCP tools must remain callable
            // even in plan mode — otherwise the skill asks questions but can
            // never execute the action.
            const PLAN_ALLOWED: &[&str] = &["ExitPlanMode", "WebFetch", "WebSearch"];
            let discovered = self.discovered_tools.lock().unwrap().clone();
            filtered.retain(|t| {
                t.is_read_only()
                    || PLAN_ALLOWED.contains(&t.name())
                    || discovered_has(&discovered, t.name())
            });
        }

        // DAG mode: read-only research + dispatch tools only. The agent
        // designs a DAG task graph but cannot edit files or run shell commands.
        if is_dag {
            const DAG_ALLOWED: &[&str] = &[
                "EnterPlanMode",
                "WebFetch",
                "WebSearch",
                // Native dispatch tools:
                "DispatchListAgents",
                "DispatchCreateParent",
                "DispatchCreateParentAndRun",
                "DispatchTask",
                "DispatchAllTasks",
                // MCP fallback (backward compat during migration):
                "mcp__senclaw-dispatch__list_agents",
                "mcp__senclaw-dispatch__create_parent",
                "mcp__senclaw-dispatch__create_parent_and_run",
                "mcp__senclaw-dispatch__dispatch_task",
                "mcp__senclaw-dispatch__dispatch_all_tasks",
                // Virtual persona tools:
                "mcp__senclaw-virtual__list_personas",
                "mcp__senclaw-virtual__run_persona",
            ];
            filtered.retain(|t| {
                let name = t.name();
                t.is_read_only()
                    || DAG_ALLOWED.contains(&name)
                    || name.starts_with("Dispatch")
                    || name.starts_with("mcp__senclaw-dispatch__")
            });
        }

        // `task_done` is the completion-enforcement "submit" signal for Plan and
        // Dag workflows only. In Agent mode there is no enforced final-step, so
        // the tool must NOT be offered — otherwise the model may call it and hit
        // a confusing "Missing required field: summary" error mid-conversation.
        if !is_plan && !is_dag {
            filtered.retain(|t| t.name() != crate::tools::TASK_DONE_TOOL_NAME);
        }

        // Layer 5 — defer filter. Deferred tools are excluded unless either:
        //   - they opted into `always_load()` (e.g. ToolSearch itself), OR
        //   - the model has already discovered them via a prior `ToolSearch`
        //     call this session. Discovery flips the tool into the active set
        //     so subsequent turns can actually invoke it. Mirrors claude-code's
        //     "lazy load" UX without breaking the dispatch lookup.
        let discovered = self.discovered_tools.lock().unwrap().clone();
        filtered.retain(|t| {
            t.always_load() || !t.should_defer() || discovered_has(&discovered, t.name())
        });

        // Layer 6 — stable sort for prompt-cache stability.
        filtered.sort_by(|a, b| a.name().cmp(b.name()));

        filtered
    }

    /// Return tools currently marked `should_defer() && !always_load()`. These
    /// are NOT sent in the initial prompt — the LLM finds them through
    /// `ToolSearch`. Layer 1-4 filters (`use_tools`, Plan, cowork) still apply
    /// so admins can completely disable a tool, not just defer it.
    pub fn deferred_tools(&self) -> Vec<Arc<dyn Tool>> {
        let opts = self.options.read().unwrap();
        let use_tools = &opts.use_tools;
        let is_plan = opts.agent_mode == AgentMode::Plan;
        let is_dag = opts.agent_mode == AgentMode::Dag;
        // Same rename decoration as `tools_for_main_agent` so ToolSearch and
        // the deferred-tools reminder show the aliased names.
        let tools =
            crate::tools::tool_alias::apply_alias_names(self.builtin_tools.read().unwrap().clone());

        // Resolve the whitelist like `tools_for_main_agent` does — a naive
        // exact match on `use_tools` silently drops every MCP tool listed by
        // short name (persona says `browser_navigate`, the tool registers as
        // `mcp__senclaw-browser__browser_navigate`), which makes those tools
        // invisible even to ToolSearch.
        let keep: Option<std::collections::HashSet<String>> = if use_tools.is_empty() {
            None
        } else {
            let mut k = std::collections::HashSet::new();
            for entry in use_tools.iter() {
                if let Some(t) =
                    crate::tools::tool_search::resolve_tool_by_name(entry, tools.as_slice())
                {
                    k.insert(t.name().to_string());
                }
            }
            Some(k)
        };

        let mut deferred: Vec<Arc<dyn Tool>> = tools
            .iter()
            .filter(|t| {
                let name = t.name();
                if let Some(keep) = &keep {
                    if !keep.contains(name) {
                        return false;
                    }
                }
                if is_plan && name == "TodoWrite" {
                    return false;
                }
                // In DAG mode, only allow read-only + dispatch tools in deferred set.
                if is_dag
                    && !t.is_read_only()
                    && !name.starts_with("Dispatch")
                    && !name.starts_with("mcp__senclaw-dispatch__")
                    && !name.starts_with("mcp__senclaw-virtual__")
                {
                    return false;
                }
                t.should_defer() && !t.always_load()
            })
            .cloned()
            .collect();
        deferred.sort_by(|a, b| a.name().cmp(b.name()));
        deferred
    }

    /// Union of active + deferred tools — everything the agent could invoke this
    /// turn (after `use_tools` / Plan / DAG filters), deduped by name. Backs
    /// `ToolSearch`'s `select:` path so naming an already-active tool (e.g.
    /// `Skill`) confirms it's callable instead of returning a misleading
    /// "0 matches" that sends the model into a retry spiral.
    pub fn available_tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut out = self.tools_for_main_agent();
        let mut seen: std::collections::HashSet<String> =
            out.iter().map(|t| t.name().to_string()).collect();
        for t in self.deferred_tools() {
            if seen.insert(t.name().to_string()) {
                out.push(t);
            }
        }
        out
    }

    /// Backwards-compatible alias used by existing call sites. Prefer
    /// [`Self::tools_for_main_agent`] in new code.
    pub fn get_tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools_for_main_agent()
    }

    /// Resolve the tool list for a **subagent** turn. Applies all main-agent
    /// filters then layers two more:
    ///
    ///   5. **`SUBAGENT_EXCLUDED_TOOLS`** — strip Task / bg-job / picker /
    ///      plan-exit / todo tools (subagents must not spawn nested subagents
    ///      and can't surface UI prompts). Mirrors sema-core's
    ///      `SUBAGENT_EXCLUDED_TOOLS` set — saves ~7 tool definitions per
    ///      subagent turn.
    ///   6. **`agent_tools` whitelist** — when provided and not `["*"]`, keep
    ///      only the tools the subagent persona is declared to use. This is
    ///      the per-persona `tools` field from the agent config.
    ///
    /// Returns an empty list if every tool was filtered out.
    pub fn tools_for_subagent(&self, agent_tools: Option<&[String]>) -> Vec<Arc<dyn Tool>> {
        use crate::zen_core::prompt::SUBAGENT_EXCLUDED_TOOLS;
        let mut tools = self.tools_for_main_agent();
        tools.retain(|t| !SUBAGENT_EXCLUDED_TOOLS.contains(&t.name()));
        if let Some(allowed) = agent_tools {
            if !allowed.iter().any(|t| t == "*") {
                let set: std::collections::HashSet<&str> =
                    allowed.iter().map(|s| s.as_str()).collect();
                tools.retain(|t| set.contains(t.name()));
            }
        }
        tools
    }

    // ============================================================
    // Event helpers — fire event on bus AND call registered handler
    // ============================================================

    fn fire(&self, event: EngineEvent) {
        self.event_bus.emit(event.clone());
        let handlers = self.handlers.read().unwrap();
        match event {
            EngineEvent::SessionReady(d) => {
                if let Some(ref h) = handlers.on_session_ready {
                    h(d);
                }
            }
            EngineEvent::MessageComplete(d) => {
                if let Some(ref h) = handlers.on_message_complete {
                    h(d);
                }
            }
            EngineEvent::StateUpdate(d) => {
                if let Some(ref h) = handlers.on_state_update {
                    h(d);
                }
            }
            EngineEvent::SessionError(d) => {
                if let Some(ref h) = handlers.on_session_error {
                    h(d);
                }
            }
            EngineEvent::SessionInterrupted(d) => {
                if let Some(ref h) = handlers.on_session_interrupted {
                    h(d);
                }
            }
            EngineEvent::TodosUpdate(d) => {
                if let Some(ref h) = handlers.on_todos_update {
                    h(d);
                }
            }
            EngineEvent::ConversationUsage(d) => {
                if let Some(ref h) = handlers.on_conversation_usage {
                    h(d);
                }
            }
            EngineEvent::CompactStart(d) => {
                if let Some(ref h) = handlers.on_compact_start {
                    h(d);
                }
            }
            EngineEvent::CompactExec(d) => {
                if let Some(ref h) = handlers.on_compact_exec {
                    h(d);
                }
            }
            EngineEvent::ToolPermissionRequest(d) => {
                if let Some(ref h) = handlers.on_tool_permission_request {
                    h(d);
                }
            }
            EngineEvent::ToolExecutionComplete(d) => {
                if let Some(ref h) = handlers.on_tool_execution_complete {
                    h(d);
                }
            }
            EngineEvent::ToolExecutionError(d) => {
                if let Some(ref h) = handlers.on_tool_execution_error {
                    h(d);
                }
            }
            EngineEvent::AskQuestionRequest(d) => {
                if let Some(ref h) = handlers.on_ask_question_request {
                    h(d);
                }
            }
            EngineEvent::PlanExitRequest(d) => {
                if let Some(ref h) = handlers.on_plan_exit_request {
                    h(d);
                }
            }
            EngineEvent::TaskAgentStart(d) => {
                if let Some(ref h) = handlers.on_task_agent_start {
                    h(d);
                }
            }
            EngineEvent::TaskAgentEnd(d) => {
                if let Some(ref h) = handlers.on_task_agent_end {
                    h(d);
                }
            }
            EngineEvent::TextChunk(d) => {
                if let Some(ref h) = handlers.on_text_chunk {
                    h(d);
                }
            }
            EngineEvent::ThinkingChunk(d) => {
                if let Some(ref h) = handlers.on_thinking_chunk {
                    h(d);
                }
            }
            // Internal events — bus only, no handler dispatch
            // (FormRequest is consumed by the AgentPool event loop via bus
            // subscription, like the workbench events.)
            EngineEvent::SessionCleared { .. }
            | EngineEvent::InputReceived(_)
            | EngineEvent::ToolPermissionResponse(_)
            | EngineEvent::AskQuestionResponse(_)
            | EngineEvent::FormRequest(_)
            | EngineEvent::FormResponse(_)
            // LlmUsage — consumed by the AgentPool / virtual-pool event loops
            // via bus subscription (accounting funnel, no handler dispatch).
            | EngineEvent::LlmUsage(_)
            // WidgetEmit — consumed by the AgentPool event loop via bus
            // subscription (one-way, no handler dispatch), like FormRequest.
            | EngineEvent::WidgetEmit(_)
            | EngineEvent::PlanExitResponse(_)
            | EngineEvent::PlanImplement(_)
            | EngineEvent::FileReference(_)
            | EngineEvent::TopicUpdate(_)
            | EngineEvent::ConfigNoModels(_)
            // Workbench events — consumed by WorkbenchBridge via event_bus subscription
            | EngineEvent::WorkbenchNew(_)
            | EngineEvent::WorkbenchServiceReady(_)
            | EngineEvent::WorkbenchServiceCrashed(_)
            | EngineEvent::WorkbenchServiceStopped(_) => {}
        }
    }

    // ============================================================
    // Session lifecycle — internal helpers
    // ============================================================

    fn generate_session_id() -> String {
        Uuid::new_v4().to_string()
    }

    pub fn abort_current(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(ref token) = state.current_abort {
            if !token.is_cancelled() {
                info!("Aborting current request via CancellationToken");
                token.cancel();
            }
        }
        state.current_abort = None;
    }

    /// Build `ExecuteHooksOptions` reusing the engine's HTTP client and active profile.
    fn hook_opts(&self) -> (Client, ModelProfile) {
        (self.http_client.clone(), self.resolve_model_profile())
    }

    /// Build the base fields included in every hook input payload.
    fn hook_base(&self, event: HookEvent) -> HookInputBase {
        let sid = self.session_id.read().unwrap().clone().unwrap_or_default();
        let cwd = self.options.read().unwrap().working_dir.clone();
        HookInputBase {
            hook_event_name: event,
            session_id: sid,
            agent_id: MAIN_AGENT_ID.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            cwd,
        }
    }

    /// Resolve the model profile for this engine, honoring the per-engine
    /// override (`options.model_config_id`, set from the group's `llm_config_id`).
    fn resolve_model_profile(&self) -> ModelProfile {
        let override_id = self.options.read().unwrap().model_config_id.clone();
        Self::resolve_model_profile_with(override_id.as_deref())
    }

    /// Whether the model a turn would run on accepts image blocks.
    ///
    /// Deliberately routed through [`Self::resolve_model_profile_at`] rather
    /// than re-deriving the config: the answer has to match the model that
    /// actually receives the turn, including the quirks of that resolution (an
    /// unknown `override_id` falls back to the active config, an empty override
    /// list falls back to the first entry). A separate lookup would disagree on
    /// exactly those edges and route a vision model's images through OCR.
    ///
    /// The explicit `vision` toggle from Settings → Models wins; otherwise the
    /// model name is matched against [`vision::infer_vision`].
    pub(crate) fn model_accepts_images(
        config_path: &std::path::Path,
        override_id: Option<&str>,
    ) -> bool {
        vision::model_has_vision(&Self::resolve_model_profile_at(config_path, override_id))
    }

    /// Pure selection step: pick the `LlmConfig` for `override_id` (per-group
    /// model), falling back to the globally active id. Returns `None` when
    /// neither matches a known config (caller then defaults to the first entry).
    fn select_llm_config(
        loaded: &crate::gateway::group_manager::LlmConfigResult,
        override_id: Option<&str>,
    ) -> Option<crate::gateway::group_manager::LlmConfig> {
        override_id
            .and_then(|id| loaded.configs.iter().find(|c| c.id == id))
            .or_else(|| {
                loaded
                    .active_id
                    .as_ref()
                    .and_then(|id| loaded.configs.iter().find(|c| &c.id == id))
            })
            .cloned()
    }

    /// Resolve a model profile against the global config.
    ///
    /// Resolution order:
    /// 1. `override_id` (per-group selection) matched against `llmConfigs`.
    /// 2. Globally active model (`activeLlmConfigId`).
    /// 3. First config in the list.
    /// 4. Legacy env-based setup (`SENCLAW_OPENAI_*`).
    fn resolve_model_profile_with(override_id: Option<&str>) -> ModelProfile {
        let config_path = std::env::var("SENCLAW_CONFIG_PATH")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| {
                        h.join(".senclaw")
                            .join("config.json")
                            .to_string_lossy()
                            .to_string()
                    })
                    .unwrap_or_else(|| ".senclaw/config.json".to_string())
            });
        Self::resolve_model_profile_at(std::path::Path::new(&config_path), override_id)
    }

    /// Same resolution against an explicit `config.json`.
    ///
    /// Split out so callers holding a [`Config`](crate::config::Config) — and
    /// tests — can ask about the profile a turn would use without going through
    /// the process-global `SENCLAW_CONFIG_PATH`.
    pub(crate) fn resolve_model_profile_at(
        config_path: &std::path::Path,
        override_id: Option<&str>,
    ) -> ModelProfile {
        let loaded = load_llm_configs(config_path);
        if !loaded.configs.is_empty() {
            let selected = Self::select_llm_config(&loaded, override_id)
                .unwrap_or_else(|| loaded.configs[0].clone());

            let provider = if selected.provider.trim().is_empty() {
                if selected.adapt.trim().eq_ignore_ascii_case("anthropic") {
                    "anthropic".to_string()
                } else {
                    "openai".to_string()
                }
            } else {
                selected.provider.clone()
            };

            // An OAuth-backed config stores only an account id; the bearer
            // token comes from the OAuth store. This function is synchronous
            // and cannot await a refresh, so it takes whatever the background
            // refresher last cached — a token that expired in between is
            // handled by the 401 retry in the transport layer.
            let (api_key, oauth_provider, oauth_account_id) = if selected.is_oauth() {
                let account_id = selected.oauth_account_id.clone().unwrap_or_default();
                let token =
                    crate::providers::oauth::access_token_for(&account_id).unwrap_or_default();
                let provider_id = crate::providers::oauth::global()
                    .and_then(|m| m.account(&account_id))
                    .map(|a| a.provider);
                (token, provider_id, Some(account_id))
            } else {
                (selected.api_key, None, None)
            };

            return ModelProfile {
                name: selected.label,
                provider,
                model_name: selected.model_name,
                base_url: selected.base_url,
                api_key,
                max_tokens: selected.max_tokens,
                context_length: selected.context_length,
                adapt: if selected.adapt.trim().is_empty() {
                    None
                } else {
                    Some(selected.adapt)
                },
                // Carry the user's explicit vision flag through instead of
                // dropping it; `None` still means "infer from the model name".
                vision: selected.vision,
                edit_format: selected.edit_format,
                oauth_provider,
                oauth_account_id,
            };
        }

        // Fallback for legacy env-based setup.
        let base_url = std::env::var("SENCLAW_OPENAI_BASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                std::env::var("OPENAI_BASE_URL")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .unwrap_or_else(|| "https://api.openai.com/v1".into());
        let api_key = std::env::var("SENCLAW_OPENAI_API_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                std::env::var("OPENAI_API_KEY")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .unwrap_or_default();
        let model_name = std::env::var("SENCLAW_OPENAI_CHAT_MODEL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "gpt-4o-mini".into());

        ModelProfile {
            name: "default".into(),
            provider: "openai".into(),
            model_name,
            base_url,
            api_key,
            max_tokens: 4096,
            context_length: 128000,
            adapt: Some("openai".into()),
            ..Default::default()
        }
    }

    // ============================================================
    // Context management for multi-tenant support
    // ============================================================

    /// Create an EngineStore from this engine's current state.
    ///
    /// This can be used with `run_with_engine()` to execute operations
    /// within this engine's context, enabling automatic access to
    /// the engine's resources without explicit passing.
    pub fn create_engine_store(&self, profile: ModelProfile) -> super::EngineStore {
        super::EngineStore {
            instance_id: self.instance_id.clone(),
            working_dir: self.options.read().unwrap().working_dir.clone(),
            agent_data_dir: self.options.read().unwrap().agent_data_dir.clone(),
            core_config: super::CoreConfig {
                model_profile: profile,
                thinking: self.options.read().unwrap().thinking,
                stream: self.options.read().unwrap().stream,
                agent_mode: self.options.read().unwrap().agent_mode.as_str().to_string(),
                use_tools: self.options.read().unwrap().use_tools.clone(),
            },
            event_bus: self.event_bus.clone(),
            state_manager: Arc::clone(&self.state),
            mcp_manager: self.mcp_manager.clone(),
            hook_manager: self.hook_manager.clone(),
        }
    }

    /// Run an operation within this engine's context.
    ///
    /// This is a convenience method that combines `create_engine_store()`
    /// with `run_with_engine()`.
    pub async fn run_in_context<F, Fut, T>(&self, profile: ModelProfile, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T> + Send,
        T: Send + 'static,
    {
        let store = self.create_engine_store(profile);
        super::run_with_engine(store, f).await
    }
}

// ============================================================================
// ZenCore trait impl
// ============================================================================

impl ZenCore for ZenEngine {
    fn create_session(&self, session_id: Option<&str>) -> Result<()> {
        info!("[{}] create_session", self.instance_id);

        // Abort any in-flight request
        self.abort_current();

        // Clear state
        let mut state = self.state.lock().unwrap();
        state.clear_all();

        // Set session id
        let sid = session_id
            .map(|s| s.to_owned())
            .unwrap_or_else(Self::generate_session_id);
        state.set_session_id(sid.clone());
        *self.session_id.write().unwrap() = Some(sid.clone());

        // Hydrate LLM trajectory from disk so stop / daemon restart do not
        // erase session memory while the UI transcript is still present.
        // Callers that intend a hard wipe (`stop_and_clear`, `/reset`) must
        // `session_store::clear` *before* this method.
        let mut history_loaded = false;
        if let Some(msgs) = super::session_store::load(&self.instance_id) {
            let n = msgs.len();
            state.set_message_history(MAIN_AGENT_ID, msgs);
            history_loaded = true;
            self.identity_refresh_pending.store(true, Ordering::Relaxed);
            info!(
                "[{}] create_session: hydrated {n} LLM message(s) from session store",
                self.instance_id
            );
        } else {
            self.identity_refresh_pending
                .store(false, Ordering::Relaxed);
        }
        drop(state);

        // Register working dir in ConfigManager (creates default config if new)
        let working_dir = self.options.read().unwrap().working_dir.clone();
        config_manager::with_conf_manager(|mgr| mgr.register_project(&working_dir));

        // Initialize plugins (skills + custom commands)
        self.initialize_plugins();

        // Fire SessionStart hook (fire-and-forget — non-blockable)
        if self
            .hook_manager
            .has_hooks_for_event(&HookEvent::SessionStart)
        {
            let hm = self.hook_manager.clone();
            let (client, profile) = self.hook_opts();
            let base = self.hook_base(HookEvent::SessionStart);
            tokio::spawn(async move {
                zen_hooks::execute_hooks(
                    &hm,
                    &HookEvent::SessionStart,
                    &HookInput::Session(SessionInput { base }),
                    &ExecuteHooksOptions {
                        client: Some(&client),
                        profile: Some(&profile),
                        ..Default::default()
                    },
                )
                .await;
            });
        }

        // Emit session:ready
        let opts = self.options.read().unwrap();
        self.fire(EngineEvent::SessionReady(SessionReadyData {
            working_dir: opts.working_dir.clone(),
            session_id: sid,
            history_loaded,
            usage: UsageData {
                use_tokens: 0,
                max_tokens: 0,
                prompt_tokens: 0,
            },
            project_input_history: Vec::new(),
        }));

        // Transition main agent to idle
        let mut state = self.state.lock().unwrap();
        state.update_state(MAIN_AGENT_ID, SessionState::Idle);
        self.fire(EngineEvent::StateUpdate(StateUpdateData {
            state: SessionState::Idle,
        }));

        Ok(())
    }

    fn process_user_input(&self, prompt: &str, original_input: Option<&str>) -> Result<()> {
        self.process_user_input_with_images(prompt, original_input, Vec::new())
    }

    fn process_user_input_with_images(
        &self,
        prompt: &str,
        original_input: Option<&str>,
        images: Vec<ImageSource>,
    ) -> Result<()> {
        info!(
            "[{}] process_user_input: {} ({} image block(s))",
            self.instance_id,
            prompt,
            images.len()
        );

        // Queue when a turn is already in flight (mirrors TS SemaEngine
        // pending-input queue): `/commands` run solo in a later turn; injects
        // may be appended to the running turn's tool results mid-flight or
        // batched into the next turn. Check-and-transition happens under one
        // lock so two concurrent inputs can't both start loops.
        {
            let mut state = self.state.lock().unwrap();
            if state.current_state(MAIN_AGENT_ID) == SessionState::Processing {
                let queued = Self::push_pending(&mut state, prompt, original_input);
                drop(state);
                if !images.is_empty() {
                    // The pending queue carries text only. Callers dispatch
                    // image turns through the per-group queue precisely so this
                    // can't happen; say so out loud if it ever does, rather than
                    // letting the model answer about an image it never got.
                    warn!(
                        "[{}] queued mid-turn input dropped {} image block(s) — the pending-input queue is text-only",
                        self.instance_id,
                        images.len()
                    );
                }
                self.report_queued(prompt, queued);
                return Ok(());
            }
            state.update_state(MAIN_AGENT_ID, SessionState::Processing);
        }
        self.fire(EngineEvent::StateUpdate(StateUpdateData {
            state: SessionState::Processing,
        }));
        self.fire(EngineEvent::InputReceived(InputReceivedData {
            input: prompt.to_string(),
            queued: false,
            inject: false,
            queue_length: 0,
        }));

        self.start_query(prompt, images)
    }

    fn pause_session(&self) {
        info!("[{}] pause_session", self.instance_id);
        self.abort_current();
        let mut state = self.state.lock().unwrap();
        state.update_state(MAIN_AGENT_ID, SessionState::Paused);
        self.fire(EngineEvent::StateUpdate(StateUpdateData {
            state: SessionState::Paused,
        }));
    }

    fn interrupt_session(&self, target_state: SessionState) {
        info!(
            "[{}] interrupt_session → {:?}",
            self.instance_id, target_state
        );
        self.abort_current();
        let mut state = self.state.lock().unwrap();
        state.update_state(MAIN_AGENT_ID, target_state);
        self.fire(EngineEvent::StateUpdate(StateUpdateData {
            state: target_state,
        }));
    }

    fn dispose(&self) {
        info!("[{}] dispose", self.instance_id);
        self.abort_current();
        self.state.lock().unwrap().clear_all();
        self.response_registry.clear();
        self.workbench_service.shutdown();
        self.discovered_tools.lock().unwrap().clear();

        // Fire SessionEnd hook (non-blockable, fire-and-forget)
        if self
            .hook_manager
            .has_hooks_for_event(&HookEvent::SessionEnd)
        {
            let hm = self.hook_manager.clone();
            let (client, profile) = self.hook_opts();
            let base = self.hook_base(HookEvent::SessionEnd);
            tokio::spawn(async move {
                zen_hooks::execute_hooks(
                    &hm,
                    &HookEvent::SessionEnd,
                    &HookInput::Session(SessionInput { base }),
                    &ExecuteHooksOptions {
                        client: Some(&client),
                        profile: Some(&profile),
                        ..Default::default()
                    },
                )
                .await;
            });
        }
    }

    fn set_working_dir(&self, dir: &str) {
        info!("[{}] set_working_dir: {dir}", self.instance_id);
        self.options.write().unwrap().working_dir = dir.to_owned();
    }

    fn clear_working_dir(&self) {
        info!("[{}] clear_working_dir", self.instance_id);
    }

    fn update_skip_permissions(&self, skip: bool) {
        {
            let mut opts = self.options.write().unwrap();
            opts.skip_file_edit_permission = skip;
            opts.skip_bash_exec_permission = skip;
            opts.skip_skill_permission = skip;
            opts.skip_mcp_tool_permission = skip;
        }
        // Propagate to the live checker — PermissionManager reads its own
        // flags, not the options copy.
        self.permission_manager
            .update_skip_flags(skip, skip, skip, skip);
    }

    fn update_thinking(&self, enabled: bool) {
        self.options.write().unwrap().thinking = enabled;
    }

    fn set_use_tools(&self, tools: Vec<String>) {
        info!("[{}] set_use_tools: {:?}", self.instance_id, tools);
        self.options.write().unwrap().use_tools = tools;
    }

    fn reload_skills(&self, disabled: &[String]) {
        info!(
            "[{}] reload_skills ({} disabled)",
            self.instance_id,
            disabled.len()
        );
        let config = crate::config::Config::from_env();
        let mut entries = crate::skills::scan::load_all_local_skills(&config);
        if !disabled.is_empty() {
            entries.retain(|e| !disabled.iter().any(|d| d == &e.name));
        }
        self.skill_registry.load_entries(&entries);
        info!(
            "[{}] reload_skills: {} skills loaded",
            self.instance_id,
            self.skill_registry.len()
        );
    }

    fn has_session_tool_results(&self) -> bool {
        let state = self.state.lock().unwrap();
        let history = state.message_history(MAIN_AGENT_ID);
        history.iter().any(|msg| {
            if msg.msg_type != "assistant" {
                return false;
            }
            msg.message
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
        })
    }

    fn add_or_update_mcp_server(&self, cfg: &McpServerConfig, _scope: &str) -> Result<()> {
        info!(
            "[{}] add_or_update_mcp_server: {}",
            self.instance_id, cfg.name
        );
        let instance_id = self.instance_id.clone();
        let registry = self.mcp_registry.clone();
        let builtin_tools_lock = Arc::new(std::sync::RwLock::<Vec<Arc<dyn Tool>>>::new(vec![]));
        // We can't move self into a 'static future. Instead we use a Mutex-protected
        // Vec that we'll merge into builtin_tools synchronously after spawn returns.
        // Simpler approach: use a oneshot channel to send bridge_tools back.
        let name = cfg.name.clone();
        let command = cfg.command.clone();
        let args = cfg.args.clone();
        let env = cfg.env.clone();
        let request_timeout_secs = cfg.request_timeout_secs;
        // Shared buffer for the spawned task to write tool objects into.
        let tools_buffer: Arc<Mutex<Vec<Arc<dyn Tool>>>> = Arc::new(Mutex::new(Vec::new()));
        let tools_buffer_clone = Arc::clone(&tools_buffer);
        let _ = builtin_tools_lock;

        let handle = tokio::spawn(async move {
            let timeout = Duration::from_secs(request_timeout_secs.unwrap_or(300));
            match registry.spawn(&name, &command, &args, &env, timeout).await {
                Ok(tool_infos) => {
                    let count = tool_infos.len();
                    let server_name = name.clone();
                    let reg_clone = registry.clone();
                    let bridge_tools: Vec<Arc<dyn Tool>> = tool_infos
                        .into_iter()
                        .map(|ti| {
                            let full_name = if server_name.starts_with("senclaw-") {
                                let clean_server = &server_name["senclaw-".len()..];
                                let mut clean_tool = ti.name.clone();
                                let prefix = format!("{}_", clean_server);
                                if clean_tool.starts_with(&prefix) {
                                    clean_tool = clean_tool[prefix.len()..].to_string();
                                }
                                format!("mcp__{}__{}", clean_server, clean_tool)
                            } else {
                                format!("mcp__{}__{}", server_name, ti.name)
                            };
                            Arc::new(McpRegistryBridgeTool {
                                full_name,
                                tool_name: ti.name,
                                server_name: server_name.clone(),
                                desc: ti.description,
                                schema: ti.input_schema,
                                registry: reg_clone.clone(),
                            }) as Arc<dyn Tool>
                        })
                        .collect();
                    *tools_buffer_clone.lock().unwrap() = bridge_tools;
                    info!("[{instance_id}] MCP {name}: {count} tool(s) spawned");
                    count
                }
                Err(e) => {
                    warn!("[{instance_id}] MCP {name} spawn failed: {e}");
                    0
                }
            }
        });

        // Synchronously wait for spawn to complete then merge tools into builtin_tools.
        // We're on a tokio thread so we block_in_place to avoid deadlocking.
        tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            let count = rt.block_on(handle).unwrap_or(0);
            if count > 0 {
                let new_tools = std::mem::take(&mut *tools_buffer.lock().unwrap());
                let prefix = format!("mcp__{}__", cfg.name);
                let mut tools = self.builtin_tools.write().unwrap();
                tools.retain(|t| !t.name().starts_with(&prefix));
                tools.extend(new_tools);
                info!(
                    "[{}] MCP {}: {count} tool(s) added to builtin_tools",
                    self.instance_id, cfg.name
                );
            }
        });
        Ok(())
    }

    fn add_allowed_tool(&self, key: &str) {
        self.permission_manager.add_allowed_tool(key);
    }

    fn respond_to_tool_permission(&self, response: ToolPermissionResponseData) {
        self.response_registry.deliver_tool_permission(response);
    }

    fn respond_to_ask_question(&self, response: AskQuestionResponseData) {
        self.response_registry.deliver_ask_question(response);
    }

    fn respond_to_form(&self, response: FormResponseData) {
        self.response_registry.deliver_form(response);
    }

    fn respond_to_plan_exit(&self, response: PlanExitResponseData) {
        tracing::info!(
            "[{}] plan exit response: agent={} selected={}",
            self.instance_id,
            response.agent_id,
            response.selected
        );
        // Deliver the user's choice to the suspended `ExitPlanMode` tool. The
        // tool registered a waiter via `register_ask_question(agent_id)` and
        // reads `answers["selected"]`, so we shape the response accordingly.
        // Without this delivery the tool blocks forever and the agent hangs.
        let mut answers = std::collections::HashMap::new();
        answers.insert("selected".to_string(), response.selected.clone());
        self.response_registry
            .deliver_ask_question(AskQuestionResponseData {
                agent_id: response.agent_id.clone(),
                answers,
            });

        // On approval, flip back to Agent mode so the agent can actually
        // execute the plan (read-only tool filtering is lifted). On
        // "cancelled" we stay in Plan mode — the user rejected, nothing to do.
        match response.selected.as_str() {
            "startEditing" | "clearContextAndStart" => {
                self.update_agent_mode(AgentMode::Agent);
            }
            _ => {}
        }

        // Keep the event-bus emit so any observers (logging, future hooks)
        // still see the resolution.
        self.fire(EngineEvent::PlanExitResponse(response));
    }

    fn set_handlers(&self, handlers: ZenCoreHandlers) {
        *self.handlers.write().unwrap() = handlers;
    }

    fn update_agent_mode(&self, mode: AgentMode) {
        let mut opts = self.options.write().unwrap();
        let changed = opts.agent_mode != mode;
        let prev = opts.agent_mode;
        opts.agent_mode = mode;
        if changed {
            tracing::info!(
                "[{}] agent_mode: {} → {}",
                self.instance_id,
                prev.as_str(),
                mode.as_str()
            );
            if mode == AgentMode::Plan {
                let mut state = self.state.lock().unwrap();
                state.reset_plan_mode_info_sent();
            }
            // DAG mode: pre-discover dispatch tools so they appear in the
            // active tool roster immediately (bypass should_defer filter).
            if mode == AgentMode::Dag {
                let mut disc = self.discovered_tools.lock().unwrap();
                for name in &[
                    "DispatchListAgents",
                    "DispatchCreateParent",
                    "DispatchCreateParentAndRun",
                    "DispatchTask",
                    "DispatchAllTasks",
                ] {
                    disc.pin((*name).to_string());
                }
                tracing::info!(
                    "[{}] DAG mode: pre-discovered dispatch tools",
                    self.instance_id
                );
            }
        }
    }

    fn get_tool_infos(&self) -> Vec<ToolInfo> {
        self.get_tools()
            .iter()
            .map(|t| ToolInfo {
                name: t.name().to_string(),
                description: t.description().to_string(),
                status: "enable".to_string(),
            })
            .collect()
    }
}

// ============================================================================
// Plugin initialization (skills + custom commands)
// ============================================================================

impl ZenEngine {
    /// Persist the main-agent LLM trajectory for this chat JID.
    fn persist_llm_history(jid: &str, messages: &[Message]) {
        if let Err(e) = super::session_store::save(jid, messages) {
            warn!(jid, error = %e, "failed to persist LLM session history");
        }
    }

    /// Wipe both RAM and disk history for this chat (hard reset).
    pub fn wipe_persisted_history(&self) {
        let _ = super::session_store::clear(&self.instance_id);
        let mut state = self.state.lock().unwrap();
        state.set_message_history(MAIN_AGENT_ID, Vec::new());
        self.identity_refresh_pending
            .store(false, Ordering::Relaxed);
    }

    /// Hot-update the agent data directory (SOUL.md / plans / memory base).
    pub fn set_agent_data_dir(&self, dir: &str) {
        info!("[{}] set_agent_data_dir: {dir}", self.instance_id);
        self.options.write().unwrap().agent_data_dir = dir.to_owned();
    }

    /// Workspace handoff / progress for resume after hydrate or compact.
    fn collect_resume_context(jid: &str) -> Option<String> {
        let ws = crate::control_plane::workspace::Workspace::for_chat(jid);
        let mut parts: Vec<String> = Vec::new();
        if let Some(handoff) = ws.read_handoff() {
            let body = crate::util::text::truncate_on_char_boundary(&handoff, 6_000);
            parts.push(format!("Session handoff (resume notes):\n{body}"));
        }
        if let Some(progress) = ws.read_progress() {
            let body = crate::util::text::truncate_on_char_boundary(&progress, 4_000);
            parts.push(format!("Session progress log:\n{body}"));
        }
        if parts.is_empty() {
            return None;
        }
        Some(format!(
            "<system-reminder>\n{}\n</system-reminder>\n\n",
            parts.join("\n\n")
        ))
    }

    /// Push one input onto the pending queue. Returns `(inject, queue_length)`.
    fn push_pending(
        state: &mut StateManager,
        prompt: &str,
        original_input: Option<&str>,
    ) -> (bool, usize) {
        let item = PendingUserInput::classify(prompt, original_input, false);
        let inject = item.kind == PendingInputKind::Inject;
        state.add_pending_input(item);
        (inject, state.pending_inputs_len())
    }

    /// Log + emit `InputReceived(queued: true)` for a just-queued input.
    fn report_queued(&self, prompt: &str, (inject, queue_length): (bool, usize)) {
        info!(
            "[{}] input queued ({}), queue length: {queue_length}",
            self.instance_id,
            if inject { "inject" } else { "command" }
        );
        self.fire(EngineEvent::InputReceived(InputReceivedData {
            input: prompt.to_string(),
            queued: true,
            inject,
            queue_length,
        }));
    }

    /// Queue an input ONLY when a turn is currently in flight. Returns `false`
    /// (does nothing) when idle — the caller should then dispatch through its
    /// normal full-turn path (e.g. GroupQueue with memory pre-retrieval).
    /// Injected inputs ride the in-flight turn's tool results, so skipping the
    /// per-turn pre-stages is correct for them.
    pub fn queue_input_if_processing(&self, prompt: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.current_state(MAIN_AGENT_ID) != SessionState::Processing {
            return false;
        }
        let queued = Self::push_pending(&mut state, prompt, None);
        drop(state);
        self.report_queued(prompt, queued);
        true
    }

    /// Build context and spawn the conversation loop for one input turn.
    /// Callers must have already set the main agent to `Processing`
    /// (`process_user_input` does; the queued-batch path arrives here with
    /// the state still `Processing` from the previous turn).
    fn start_query(&self, prompt: &str, images: Vec<ImageSource>) -> Result<()> {
        let cancel = CancellationToken::new();
        {
            let mut state = self.state.lock().unwrap();
            state.current_abort = Some(cancel.clone());
        }

        // Clone shared resources for the spawned task
        let instance_id = self.instance_id.clone();
        let engine_weak = self.self_weak.lock().unwrap().clone();
        let event_bus = self.event_bus.clone();
        let opts = self.options.read().unwrap().clone();
        let tools_initial = self.get_tools();
        // Resolver re-evaluates the live tool set each turn so ToolSearch
        // discoveries flow into subsequent turns within this same user input.
        let engine_for_tools = self.self_weak.lock().unwrap().clone();
        let tools_resolver: crate::zen_core::conversation::ToolsResolver = Arc::new(move || {
            engine_for_tools
                .upgrade()
                .map(|e| e.tools_for_main_agent())
                .unwrap_or_default()
        });
        // Keep `tools` binding for the existing log lines below.
        let _tools = tools_initial;
        // Debug: log all tools being sent to LLM so we can verify browser tools appear
        // {
        //     let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        //     let mcp_tools: Vec<&str> = tool_names
        //         .iter()
        //         .filter(|n| n.starts_with("mcp__"))
        //         .copied()
        //         .collect();
        //     info!(
        //         "[{}] get_tools: {} total ({} mcp__*): {:?}",
        //         self.instance_id,
        //         tool_names.len(),
        //         mcp_tools.len(),
        //         mcp_tools
        //     );
        // }
        let messages = {
            let state = self.state.lock().unwrap();
            state.message_history(MAIN_AGENT_ID)
        };
        let http_client = self.http_client.clone();
        let permission_manager = self.permission_manager.clone();
        let response_registry = self.response_registry.clone();
        let state_for_spawn = self.state.clone();
        let identity_refresh_for_spawn = self.identity_refresh_pending.clone();

        // Build system prompt (stable base + dynamic system context appended).
        // When the Skill tool is registered we append a skills reminder so the
        // LLM can auto-trigger skills by metadata (`name` + `description` +
        // `when-to-use`) — mirrors sema-core `generateSkillsReminder`.
        let has_skill_tool = self
            .builtin_tools
            .read()
            .unwrap()
            .iter()
            .any(|t| t.name() == "Skill");
        // Resolve profile: per-group override → active UI config → env fallback.
        // Resolved before the prompt is assembled because the model's edit
        // format (Aider-style) decides whether the prompt tells it to send
        // unified diffs or whole files, and its context window sizes the
        // skills list.
        let profile = self.resolve_model_profile();
        let skills_reminder = if has_skill_tool {
            self.build_skills_reminder(profile.context_length)
        } else {
            None
        };
        // Always-on skills (`use: always`) are injected in full every turn,
        // independent of whether the Skill tool is registered.
        let always_skills_block = self.build_always_skills_block();
        // Build deferred-tools reminder so the LLM knows ToolSearch can load
        // specialized tools on demand.
        let deferred_reminder = self.build_deferred_tools_reminder();

        let plan_mode_reminder = match opts.agent_mode {
            AgentMode::Plan => {
                let plans_dir = std::path::Path::new(&opts.agent_data_dir)
                    .join(".sema")
                    .join("plans")
                    .join("") // ensure trailing slash
                    .to_string_lossy()
                    .to_string();
                tracing::info!(
                    "[{}] Plan mode active — injecting plan reminder (plans_dir={})",
                    self.instance_id,
                    plans_dir
                );
                Some(crate::zen_core::prompt::plan_mode_reminder(&plans_dir))
            }
            AgentMode::Dag => {
                tracing::info!(
                    "[{}] DAG mode active — injecting DAG orchestration reminder",
                    self.instance_id
                );
                Some(crate::zen_core::prompt::dag_mode_reminder())
            }
            AgentMode::Agent => None,
        };

        // User-authored operating rules (`~/.senclaw/AGENTS.md`). Read per
        // turn so an edit takes effect without a restart; the file is small
        // and usually absent.
        let operating_rules =
            crate::user_profile::operating_rules_block(&crate::config::Config::from_env());

        // Repo map: ranked outline of the working directory when it is a
        // project (git repo / manifest). Answers from a process-wide cache and
        // never blocks the turn — the first turn in a fresh tree gets none
        // while the index is built in the background.
        let repo_map_block = crate::repo_map::map_for_prompt(&opts.working_dir, prompt);

        // Where this project keeps documentation, when the project keeps any.
        let project_docs_block = crate::zen_core::prompt::project_docs_block(&opts.working_dir);

        let edit_format_reminder = profile
            .edit_format
            .and_then(|f| f.prompt_reminder())
            .map(str::to_string);

        let soul_block = Self::load_soul_prompt_block(&opts.agent_data_dir);

        let system_prompt = Self::assemble_system_prompt(
            &opts.system_prompt,
            &opts.working_dir,
            skills_reminder.as_deref(),
            deferred_reminder.as_deref(),
            plan_mode_reminder.as_deref(),
            project_docs_block.as_deref(),
            always_skills_block.as_deref(),
            opts.user_defaults.as_deref(),
            operating_rules.as_deref(),
            repo_map_block.as_deref(),
            edit_format_reminder.as_deref(),
            soul_block.as_deref(),
        );

        // UserPromptSubmit hook — may update the prompt before it reaches the LLM.
        // Runs synchronously before the spawn so `updatedInput` can modify the prompt.
        let prompt = prompt.to_owned();
        let prompt = if self
            .hook_manager
            .has_hooks_for_event(&HookEvent::UserPromptSubmit)
        {
            let hm = self.hook_manager.clone();
            let (client, hook_profile) = self.hook_opts();
            let base = self.hook_base(HookEvent::UserPromptSubmit);
            let p = prompt.clone();
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    zen_hooks::execute_hooks(
                        &hm,
                        &HookEvent::UserPromptSubmit,
                        &HookInput::UserPromptSubmit(UserPromptSubmitInput { base, prompt: p }),
                        &ExecuteHooksOptions {
                            client: Some(&client),
                            profile: Some(&hook_profile),
                            ..Default::default()
                        },
                    )
                    .await
                })
            });
            if let Some(updated) = result.updated_input {
                updated["prompt"].as_str().unwrap_or(&prompt).to_owned()
            } else {
                prompt
            }
        } else {
            prompt
        };

        // Persist prompt to project history (raw, including any `#skill` prefix).
        let working_dir_for_hist = self.options.read().unwrap().working_dir.clone();
        config_manager::with_conf_manager(|mgr| {
            mgr.save_user_input_to_history(&working_dir_for_hist, &prompt);
        });

        // Explicit skill directive: a prompt that starts with `#skill-name` or
        // `/skill-name` is a hard, deterministic request to run that skill —
        // it must not depend on the model noticing the convention (weak models
        // ignore it). Strip the directive, force-inject the skill instructions,
        // and pre-discover the deferred MCP tools the skill references so they
        // are immediately callable.
        let (prompt, forced_skill_reminder) = match self.detect_explicit_skill(&prompt) {
            Some((skill_name, rest)) => {
                let reminder = self.force_skill_reminder(&skill_name);
                let body = if rest.trim().is_empty() {
                    prompt.clone()
                } else {
                    rest
                };
                (body, reminder)
            }
            None => (prompt, None),
        };

        // Build the user message (after hooks may have modified prompt).
        // On the very first turn inject volatile context (SENCLAW.md, date) as a
        // hidden <system-reminder> block so it doesn't destabilise the system prompt
        // for prompt caching.
        //
        // SENCLAW.md is only relevant for sessions that operate on a real
        // workspace (code editing, cowork). For regular chat JIDs (`web:*`,
        // `app:*`, `virtual:*`, etc.) the project doc is noise — wastes ~2k
        // tokens per turn and dilutes the model's attention from the actual
        // user query. Date-only context is still injected for everyone.
        let user_msg = {
            let mut blocks = Vec::<ContentBlock>::new();
            let refresh_identity = messages.is_empty()
                || self
                    .identity_refresh_pending
                    .swap(false, Ordering::Relaxed);
            if refresh_identity {
                let include_project_doc = Self::instance_uses_workspace(&self.instance_id);
                if let Some(ctx) = Self::collect_first_turn_context(
                    &opts.working_dir,
                    include_project_doc,
                    &self.instance_id,
                ) {
                    blocks.push(ContentBlock::Text { text: ctx });
                }
                // After hydrate / compact, also surface workspace handoff so
                // the model can resume without the dropped middle of the
                // trajectory (control-plane external memory).
                if !messages.is_empty() {
                    if let Some(resume) =
                        Self::collect_resume_context(&self.instance_id)
                    {
                        blocks.push(ContentBlock::Text { text: resume });
                    }
                }
            }
            // Skill pre-match: scan the prompt against loaded skill triggers
            // and surface a hard recommendation if any match. Mirrors the
            // claude-code pattern of a "preferred skill" hint, but driven by
            // keyword overlap (no LLM call). The reminder is part of the user
            // message so it gets the model's full attention on the very first
            // pass — vs the skill list at the end of the system prompt which
            // the model often skims.
            // Priority: explicit `#skill` directive > pre-trigger-skill
            // force-load (when enabled and a match is found) > soft keyword hint.
            if let Some(text) =
                self.skill_block_for_turn(&prompt, forced_skill_reminder.clone(), opts.pre_trigger_skill)
            {
                blocks.push(ContentBlock::Text { text });
            }
            // Per-turn language lock. Small models can default to English or
            // Chinese even when the user writes another language. Keep this
            // reminder close to the prompt, but avoid mentioning "thinking
            // blocks" because some models copy that phrase into the visible
            // answer.
            if let Some(lang) = detect_user_language(&prompt) {
                blocks.push(ContentBlock::Text {
                    text: format!(
                        "<system-reminder>\nReply in {lang}. Do not include hidden reasoning, chain-of-thought, or thinking blocks in the final answer.\n</system-reminder>"
                    ),
                });
            }
            // Attachments go in ahead of the question: every provider we target
            // reads an image best when the text that asks about it follows it.
            for source in images {
                blocks.push(ContentBlock::Image { source });
            }
            blocks.push(ContentBlock::Text {
                text: prompt.clone(),
            });
            create_user_message(blocks)
        };
        let mut messages = messages;
        messages.push(user_msg);

        // Spawn the conversation loop — runs in background, emits events
        let event_bus_spawn = event_bus.clone();
        let hook_manager_spawn = self.hook_manager.clone();
        let session_id_spawn = self.session_id.read().unwrap().clone().unwrap_or_default();
        let cwd_spawn = opts.working_dir.clone();
        tokio::spawn(async move {
            let eb = event_bus_spawn.clone();
            // Mid-turn inject source: drains queued (non-command) inputs so
            // the conversation loop can append them to tool results.
            let state_for_inject = state_for_spawn.clone();
            let pending_inject: crate::zen_core::conversation::PendingInjectSource =
                Arc::new(move || {
                    state_for_inject
                        .lock()
                        .unwrap()
                        .consume_inject_inputs_before_next_command()
                });
            let config = conversation::QueryConfig {
                agent_id: MAIN_AGENT_ID.to_string(),
                chat_jid: instance_id.clone(),
                working_dir: opts.working_dir.clone(),
                agent_data_dir: opts.agent_data_dir.clone(),
                system_prompt: system_prompt.clone(),
                tools: tools_resolver.clone(),
                http_client: http_client.clone(),
                event_bus: event_bus_spawn,
                response_registry: Some(response_registry.clone()),
                permission_checker: permission_manager.clone(),
                profile: profile.clone(),
                thinking: opts.thinking,
                stream: opts.stream,
                is_subagent: false,
                hook_manager: Some(hook_manager_spawn.clone()),
                hook_client: Some(http_client.clone()),
                hook_profile: Some(profile.clone()),
                session_id: session_id_spawn.clone(),
                enable_cache: false,
                // `controlPlane.agentStatus` (default off: it defeats a local
                // engine's prefix cache, see `ControlPlaneSettings`) — read fresh per
                // turn like the pre-skill router/shadow specs, not injected
                // via `set_runtime_config` (a documented no-op here: "config
                // is passed via environment"). Not affected by JEV_OFF — this
                // is code-computed, no Jev call.
                agent_status: crate::gateway::group_manager::load_control_plane_settings(
                    &crate::control_plane::default_config_path(),
                )
                .agent_status,
                // Pass through the current agent_mode so the conversation loop
                // knows whether to enforce task_done (Plan/Dag only).
                agent_mode: opts.agent_mode,
                max_turns_override: opts.max_agent_turns,
                pending_inject: Some(pending_inject),
            };

            let result = conversation::query(messages, &config, &cancel).await;

            if let Ok(msgs) = &result {
                let mut st = state_for_spawn.lock().unwrap();
                st.set_message_history(MAIN_AGENT_ID, msgs.clone());
                drop(st);
                Self::persist_llm_history(&instance_id, msgs);
            }

            // After-process stage: proactively summarize/compact the completed
            // conversation so the stored context stays optimized and coherent
            // for the next turn (Claude-Code-style). Runs on a clone (lock not
            // held across the LLM call), then persists the compacted history.
            // No-op for short conversations or when cancelled.
            if opts.after_process && !cancel.is_cancelled() {
                if let Ok(msgs) = &result {
                    let compacted = conversation::compact_now(msgs.clone(), &config, &cancel).await;
                    let changed = compacted.len() != msgs.len()
                        || compacted
                            .first()
                            .map(|m| m.uuid.as_str())
                            != msgs.first().map(|m| m.uuid.as_str());
                    let mut st = state_for_spawn.lock().unwrap();
                    st.set_message_history(MAIN_AGENT_ID, compacted.clone());
                    drop(st);
                    Self::persist_llm_history(&instance_id, &compacted);
                    if changed {
                        identity_refresh_for_spawn.store(true, Ordering::Relaxed);
                    }
                }
            }

            let stop_reason = match &result {
                Ok(_) => {
                    info!("[{instance_id}] conversation loop completed");
                    None
                }
                Err(e) => {
                    let msg = e.to_string();
                    warn!("[{instance_id}] conversation loop error: {msg}");
                    let classified = query_llm::LlmError::classify(e);
                    if classified.should_emit() {
                        eb.emit(EngineEvent::SessionError(classified.to_session_error()));
                    }
                    Some(msg)
                }
            };

            // Fire Stop hook (non-blockable)
            if hook_manager_spawn.has_hooks_for_event(&HookEvent::Stop) {
                let base = HookInputBase {
                    hook_event_name: HookEvent::Stop,
                    session_id: session_id_spawn.clone(),
                    agent_id: MAIN_AGENT_ID.to_string(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    cwd: cwd_spawn.clone(),
                };
                zen_hooks::execute_hooks(
                    &hook_manager_spawn,
                    &HookEvent::Stop,
                    &HookInput::Stop(StopInput { base, stop_reason }),
                    &ExecuteHooksOptions {
                        client: Some(&http_client),
                        profile: Some(&profile),
                        ..Default::default()
                    },
                )
                .await;
            }

            // Consume inputs that queued up mid-turn: a non-empty batch chains
            // straight into the next query (state stays Processing); otherwise
            // return the state machine to Idle and signal it. (Also fixes the
            // pre-existing bug where the StateManager stayed `Processing`
            // forever — only the event was emitted.)
            let next_batch = {
                let mut st = state_for_spawn.lock().unwrap();
                // Drain even when this turn was interrupted — the TS `finally`
                // does too, so messages typed during a cancelled turn still run.
                // (A session clear empties the queue first, so nothing revives.)
                let batch = st.take_next_input_batch();
                if batch.is_empty() {
                    if !cancel.is_cancelled() {
                        st.update_state(MAIN_AGENT_ID, SessionState::Idle);
                    }
                } else {
                    // The interrupt path may have set Idle/Paused already —
                    // the chained turn needs Processing so new arrivals queue.
                    st.update_state(MAIN_AGENT_ID, SessionState::Processing);
                }
                batch
            };
            if next_batch.is_empty() {
                // Signal idle (unless cancelled)
                if !cancel.is_cancelled() {
                    eb.emit(EngineEvent::StateUpdate(StateUpdateData {
                        state: SessionState::Idle,
                    }));
                }
            } else {
                let joined = next_batch
                    .iter()
                    .map(|i| i.input.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                info!(
                    "[{instance_id}] starting next turn with {} queued input(s)",
                    next_batch.len()
                );
                match engine_weak.upgrade() {
                    Some(engine) => {
                        // Queued inputs are text-only (see the warn in
                        // process_user_input_with_images).
                        if let Err(e) = engine.start_query(&joined, Vec::new()) {
                            warn!("[{instance_id}] queued-input turn failed to start: {e}");
                            let mut st = state_for_spawn.lock().unwrap();
                            st.update_state(MAIN_AGENT_ID, SessionState::Idle);
                            eb.emit(EngineEvent::StateUpdate(StateUpdateData {
                                state: SessionState::Idle,
                            }));
                        }
                    }
                    None => {
                        warn!("[{instance_id}] engine dropped before queued inputs could run");
                    }
                }
            }
        });

        Ok(())
    }

    /// Last non-empty main-agent assistant text from persisted transcript (after `query` runs).
    pub fn last_main_assistant_visible_text(&self) -> String {
        let state = self.state.lock().unwrap();
        for msg in state.message_history(MAIN_AGENT_ID).iter().rev() {
            if msg.msg_type != "assistant" {
                continue;
            }
            let (text, _, _) = conversation::extract_content(msg);
            if !text.trim().is_empty() {
                return text;
            }
        }
        String::new()
    }

    /// Assemble the final system prompt.
    ///
    /// Structure:
    /// 1. Base prompt (caller-supplied or default) — kept stable so LLM caches it.
    /// 2. Core behavioural directives — static text, also stable.
    /// 3. System context (cwd, OS, shell, git status) — dynamic but small; appended
    ///    last so any prefix cache hit on (1)+(2) is preserved when context changes.
    fn assemble_system_prompt(
        base: &str,
        working_dir: &str,
        skills_reminder: Option<&str>,
        deferred_reminder: Option<&str>,
        plan_mode_reminder: Option<&str>,
        project_docs: Option<&str>,
        always_skills: Option<&str>,
        user_defaults: Option<&str>,
        operating_rules: Option<&str>,
        repo_map: Option<&str>,
        edit_format: Option<&str>,
        soul: Option<&str>,
    ) -> String {
        // Default to the full sema-core-compatible SYSTEM_PROMPT when caller
        // doesn't override. Matches `code-old/sema-code-core/prompt/system.ts`.
        let base = if base.trim().is_empty() {
            crate::zen_core::prompt::SYSTEM_PROMPT
        } else {
            base
        };

        let sys_ctx = Self::collect_system_context(working_dir);

        let mut out = format!("{base}\n\n# System\n{sys_ctx}");
        // Persona sits early so identity/behaviour rules shape every decision
        // before skills / tools reminders. Truncated at load time.
        if let Some(block) = soul {
            out.push_str("\n\n");
            out.push_str(block);
        }
        if let Some(reminder) = skills_reminder {
            out.push_str("\n\n");
            out.push_str(reminder);
        }
        if let Some(reminder) = deferred_reminder {
            out.push_str("\n\n");
            out.push_str(reminder);
        }
        if let Some(reminder) = plan_mode_reminder {
            out.push_str("\n\n");
            out.push_str(reminder);
        }
        if let Some(block) = project_docs {
            out.push_str("\n\n");
            out.push_str(block);
        }
        if let Some(block) = always_skills {
            out.push_str("\n\n");
            out.push_str(block);
        }
        if let Some(block) = user_defaults {
            out.push_str("\n\n");
            out.push_str(block);
        }
        // The repo map changes as files change, so it sits after the stable
        // prefix (base + reminders) to keep that prefix cacheable.
        if let Some(block) = repo_map {
            out.push_str("\n\n");
            out.push_str(block);
        }
        if let Some(block) = edit_format {
            out.push_str("\n\n");
            out.push_str(block);
        }
        // `AGENTS.md`, last. Position matters twice over: appending keeps the
        // cacheable prefix above it intact, and — the reason that is not
        // negotiable — this is text the user types. Spliced in ahead of the
        // base prompt, a line saying "ignore the rules above" would be read
        // before the safety section. The wrapper written by
        // `operating_rules_block` states that safety still wins.
        if let Some(block) = operating_rules {
            out.push_str("\n\n");
            out.push_str(block);
        }
        out
    }

    /// Load and wrap `SOUL.md` for the system prompt. Caps size so a huge
    /// persona file cannot blow the context budget alone.
    fn load_soul_prompt_block(agent_data_dir: &str) -> Option<String> {
        if agent_data_dir.trim().is_empty() {
            return None;
        }
        let path = std::path::Path::new(agent_data_dir).join("SOUL.md");
        let raw = std::fs::read_to_string(&path).ok()?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        let body = crate::util::text::truncate_on_char_boundary(
            trimmed,
            crate::user_profile::MAX_FLAT_FILE_CHARS,
        );
        Some(format!("# Persona (SOUL.md)\n{body}"))
    }

    /// Build the deferred-tools system reminder. Returns `None` when zero
    /// tools are deferred so callers skip the empty block.
    fn build_deferred_tools_reminder(&self) -> Option<String> {
        use crate::zen_core::prompt::{render_deferred_tools_reminder, DeferredToolHint};
        let deferred = self.deferred_tools();
        if deferred.is_empty() {
            return None;
        }
        // Materialize hints so we don't hold tool refs across closure bounds.
        let hints: Vec<(String, String)> = deferred
            .iter()
            .map(|t| (t.name().to_string(), t.search_hint()))
            .collect();
        let rows: Vec<DeferredToolHint<'_>> = hints
            .iter()
            .map(|(n, h)| DeferredToolHint {
                name: n.as_str(),
                search_hint: h.clone(),
            })
            .collect();
        render_deferred_tools_reminder(&rows)
    }

    /// Render the metadata-driven skills reminder block when the `Skill` tool
    /// is registered. Returns `None` when there are zero auto-invokable skills.
    fn build_skills_reminder(&self, context_length: u32) -> Option<String> {
        use crate::zen_core::prompt::{render_skills_reminder, skills_reminder_budget, SkillReminderRow};
        // Snapshot skill names then re-fetch metadata so we don't hold the
        // registry lock across the borrow into SkillReminderRow.
        let names = self.skill_registry.names();
        let skills: Vec<_> = names
            .iter()
            .filter_map(|n| self.skill_registry.find(n))
            // `use: always` skills are already injected in full by
            // `build_always_skills_block`, so don't also list them as
            // "invoke via Skill" candidates here.
            .filter(|s| s.metadata.use_mode != crate::skills::SkillUseMode::Always)
            .collect();
        let rows: Vec<SkillReminderRow<'_>> = skills
            .iter()
            .map(|s| SkillReminderRow {
                name: s.metadata.name.as_str(),
                description: s.metadata.description.as_str(),
                when_to_use: s.metadata.when_to_use.as_deref(),
                disable_model_invocation: s.metadata.disable_model_invocation,
            })
            .collect();
        render_skills_reminder(&rows, skills_reminder_budget(context_length))
    }

    /// Build the always-on skills block: every eligible skill declared
    /// `use: always` has its **full** instructions injected into the system
    /// prompt on every turn (vs trigger-mode skills, which load only on match /
    /// explicit call). Each skill's activation side effects (tool pre-discovery
    /// + env injection) are applied and its params surfaced. Returns `None` when
    /// no always-mode skill is active.
    fn build_always_skills_block(&self) -> Option<String> {
        let names = self.skill_registry.names();
        let mut sections = Vec::new();
        for n in &names {
            let Some(skill) = self.skill_registry.find(n) else {
                continue;
            };
            if skill.metadata.use_mode != crate::skills::SkillUseMode::Always {
                continue;
            }
            // Respect load-time gating (os / required env / required bins).
            if !skill.metadata.is_eligible() {
                tracing::info!(
                    "[Skill] skipping always-on skill '{}': {}",
                    skill.metadata.name,
                    skill
                        .metadata
                        .ineligible_reason()
                        .unwrap_or_else(|| "ineligible".into())
                );
                continue;
            }
            self.apply_skill_activation(&skill);
            let params = Self::format_skill_params(&skill.metadata.params);
            sections.push(format!(
                "===== SKILL: {name} =====\n{params}{content}\n===== END SKILL =====",
                name = skill.metadata.name,
                params = params,
                content = skill.content,
            ));
        }
        if sections.is_empty() {
            return None;
        }
        Some(format!(
            "<system-reminder>\n\
The following always-on skill(s) are active for every message. Follow their \
instructions whenever relevant to the user's request; the MCP tools they \
reference are already available (no ToolSearch needed).\n\n{}\n\
</system-reminder>",
            sections.join("\n\n")
        ))
    }

    /// Collect runtime system context: working dir, OS, shell, git status.
    fn collect_system_context(working_dir: &str) -> String {
        let os = std::env::consts::OS;
        let shell = std::env::var("SHELL")
            .ok()
            .and_then(|s| {
                std::path::Path::new(&s)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());

        let git_status = std::process::Command::new("git")
            .args(["-C", working_dir, "status", "--short", "--branch"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let mut lines = vec![
            format!("- Working directory: {working_dir}"),
            format!("- OS: {os}"),
            format!("- Shell: {shell}"),
        ];
        if let Some(gs) = git_status {
            lines.push(format!("- Git status:\n{}", Self::cap_git_status(&gs)));
        }
        lines.join("\n")
    }

    /// Bound the `git status` block injected into the system prompt. A dirty
    /// monorepo (many modified/untracked files) can emit hundreds of lines that
    /// are resent on every request for no benefit — the model only needs the
    /// branch line and a representative sample. Keep the first
    /// [`GIT_STATUS_MAX_LINES`] lines (the `--branch` header is always line 1)
    /// and summarize the rest.
    fn cap_git_status(gs: &str) -> String {
        const GIT_STATUS_MAX_LINES: usize = 30;
        let total = gs.lines().count();
        if total <= GIT_STATUS_MAX_LINES {
            return gs.to_string();
        }
        let kept: Vec<&str> = gs.lines().take(GIT_STATUS_MAX_LINES).collect();
        format!(
            "{}\n… {} more changed path(s) omitted (run `git status` for the full list)",
            kept.join("\n"),
            total - GIT_STATUS_MAX_LINES
        )
    }

    /// Pre-match user prompt against installed skills' `when-to-use` triggers
    /// and return a hard skill recommendation block when one matches.
    ///
    /// The block is appended to the user message (not the system prompt) so it
    /// sits adjacent to the actual query — models attend much more strongly
    /// here than to the skill list buried in the system prompt.
    ///
    /// Match heuristic:
    ///   - lowercase both sides
    ///   - require ≥3 distinct word overlaps OR an explicit quoted-trigger
    ///     ("…") substring hit
    ///   - ignore skills with `disable_model_invocation` (user-only)
    /// Detect an explicit skill directive at the start of the prompt:
    /// `#skill-name rest…` or `/skill-name rest…`. The token is resolved
    /// against loaded skills, hyphen/underscore- and case-insensitively, so
    /// `#ssh_connect`, `#ssh-connect`, and `/SSH-Connect` all match the
    /// `ssh-connect` skill. Returns the canonical skill name and the prompt
    /// with the directive token removed. Returns `None` when the leading token
    /// does not resolve to a known skill (so ordinary `/path` or `#tag` text is
    /// left untouched).
    fn detect_explicit_skill(&self, prompt: &str) -> Option<(String, String)> {
        let trimmed = prompt.trim_start();
        let marker = trimmed.chars().next()?;
        if marker != '#' && marker != '/' {
            return None;
        }
        let after = &trimmed[marker.len_utf8()..];
        let token_end = after.find(char::is_whitespace).unwrap_or(after.len());
        let token = &after[..token_end];
        if token.is_empty() {
            return None;
        }
        let canon = |s: &str| s.replace('-', "_").to_lowercase();
        let want = canon(token);
        let name = self
            .skill_registry
            .names()
            .into_iter()
            .find(|n| canon(n) == want)?;
        let rest = after[token_end..].trim_start().to_string();
        Some((name, rest))
    }

    /// Apply a skill's load-time side effects, shared by the `Skill` tool path
    /// and every force-load path (`#name`, pre-trigger-skill, `use: always`):
    ///
    /// 1. **Tool pre-discovery** — surface the deferred MCP tools whose verb
    ///    (last `__` segment) is named in the skill body, e.g. `ssh_list_hosts`
    ///    → `mcp__ssh-manager-mcp__ssh_list_hosts`, so they are callable without
    ///    a `ToolSearch`.
    /// 2. **OpenClaw env injection** — pull the skill's `skills.entries.<name>`
    ///    config (`env` + `apiKey` → `primaryEnv`) and inject it into the process
    ///    (only vars not already set), so the skill's tools actually have their
    ///    credentials/config. Mirrors [`crate::tools::skill::SkillTool::call`].
    fn apply_skill_activation(&self, skill: &crate::skills::Skill) {
        let content_lower = skill.content.to_lowercase();
        {
            let deferred = self.deferred_tools();
            let mut discovered = self.discovered_tools.lock().unwrap();
            // Full names the skill writes out resolve through the same cascade
            // a tool call does, which bridges the bundled and per-server
            // layouts: the verb match below looks for `browser_search`, so
            // `web-research` — which names its tools only as
            // `mcp__browser__search` — loaded with none of them once they
            // became `mcp__core__browser_search`.
            for mention in crate::tools::tool_search::mcp_mentions(&skill.content) {
                if let Some(t) = crate::tools::tool_search::resolve_tool_by_name(&mention, &deferred) {
                    if discovered.insert(t.name().to_string()) {
                        tracing::info!(
                            "[Skill] pre-discovered tool for '{}': {} (named {mention})",
                            skill.metadata.name,
                            t.name()
                        );
                    }
                }
            }
            for t in deferred {
                let full = t.name();
                // Match either the registered verb (`space_current_time`) or the
                // canonical stripped bridge name (`mcp__space__current_time`) the
                // skill docs use, so standardized docs still surface the tool.
                //
                // Whole identifiers only, and a bare verb only when it is
                // several words (`space_recurring_create`): a substring match
                // on any verb of 3+ letters loaded every `status`, `get` and
                // `list` tool whose word appeared anywhere in the skill's
                // prose, which is how one session reached 93 active tools.
                let verb = full.rsplit("__").next().unwrap_or(full).to_lowercase();
                let canonical = normalize_mcp_tool_name(full).to_lowercase();
                let matched = (verb.contains('_') && mentions_identifier(&content_lower, &verb))
                    || mentions_identifier(&content_lower, &canonical)
                    || mentions_identifier(&content_lower, &full.to_lowercase());
                if matched {
                    if discovered.insert(full.to_string()) {
                        tracing::info!(
                            "[Skill] pre-discovered tool for '{}': {full}",
                            skill.metadata.name
                        );
                    }
                }
            }
        }

        let cfg = crate::config::Config::from_env();
        let skills_cfg =
            crate::skills::config::SkillsRuntimeConfig::load(&cfg.paths.global_config_path);
        if let Some(entry) = skills_cfg.entry(&skill.metadata.name) {
            let injected =
                crate::skills::config::inject_env(entry, skill.metadata.primary_env.as_deref());
            if !injected.is_empty() {
                tracing::info!(
                    "[Skill] injected env for '{}': {}",
                    skill.metadata.name,
                    injected.join(", ")
                );
            }
        }
    }

    /// Render a skill's declared OpenClaw `params` as a short prompt block so the
    /// model knows what arguments the skill accepts. Empty when no params.
    fn format_skill_params(params: &[crate::skills::SkillParam]) -> String {
        if params.is_empty() {
            return String::new();
        }
        let mut s = String::from("Parameters this skill accepts:\n");
        for p in params {
            let req = if p.required { "required" } else { "optional" };
            let desc = p
                .description
                .as_deref()
                .map(|d| format!(" — {d}"))
                .unwrap_or_default();
            s.push_str(&format!("- `{}` ({}, {}){}\n", p.name, p.type_, req, desc));
        }
        s.push('\n');
        s
    }

    /// Build a hard directive that loads a skill's instructions inline,
    /// pre-discovers the deferred MCP tools it references, injects its
    /// configured env, and surfaces its params. Used when the user explicitly
    /// invokes a skill via `#name` / `/name`, or when the pre-trigger-skill
    /// stage force-loads a match — the instructions are injected
    /// deterministically so even a weak model runs the skill without having to
    /// call the `Skill` tool first.
    fn force_skill_reminder(&self, skill_name: &str) -> Option<String> {
        let skill = self.skill_registry.find(skill_name)?;
        self.apply_skill_activation(&skill);
        let params = Self::format_skill_params(&skill.metadata.params);

        Some(format!(
            "<system-reminder>\n\
The user explicitly invoked the `{name}` skill. Its instructions are loaded below — \
follow them now to fulfill the request. The MCP tools it references are already \
available; call them directly (no ToolSearch needed). Do not answer in plain text \
until you have carried out the skill's steps.\n\
\n\
{params}===== SKILL: {name} =====\n\
{content}\n\
===== END SKILL =====\n\
</system-reminder>\n\n",
            name = skill.metadata.name,
            params = params,
            content = skill.content,
        ))
    }

    /// Hot-update the pre-trigger-skill flag for this engine (set from the
    /// global `preTriggerSkill` toggle on each turn). When true, a confident
    /// keyword/trigger match force-loads the skill instead of only hinting.
    pub fn set_pre_trigger_skill(&self, enabled: bool) {
        self.options.write().unwrap().pre_trigger_skill = enabled;
    }

    /// Hand the next turn the pre-skill router's decision (see `skill_route`).
    pub fn set_skill_route(&self, route: Option<crate::skills::matching::SkillRoute>) {
        *self.skill_route.lock().unwrap() = Some(route);
    }

    /// What each skill says about when to use it — every skill the matcher
    /// may pick: not `disable-model-invocation`, and not `use: always` (those
    /// are already fully injected, and matching them would duplicate it).
    pub fn skill_cards(&self) -> Vec<crate::skills::matching::SkillCard> {
        self.skill_registry
            .names()
            .iter()
            .filter_map(|n| self.skill_registry.find(n))
            .filter(|s| !s.metadata.disable_model_invocation)
            .filter(|s| s.metadata.use_mode != crate::skills::SkillUseMode::Always)
            .map(|s| crate::skills::matching::SkillCard {
                name: s.metadata.name.clone(),
                description: s.metadata.description.clone(),
                when_to_use: s.metadata.when_to_use.clone(),
                triggers: s.metadata.triggers.clone(),
            })
            .collect()
    }

    /// Hot-update the after-process flag for this engine (set from the global
    /// `afterProcess` toggle on each turn). When true, the conversation is
    /// proactively compacted after each completed turn **if** the adaptive
    /// window threshold (~80% context) is reached.
    pub fn set_after_process(&self, enabled: bool) {
        self.options.write().unwrap().after_process = enabled;
    }

    /// Manually compact this chat's LLM trajectory (user Compact button).
    /// Spawns a background LLM summarization; no-op when history is too short
    /// or a turn is already in flight.
    pub fn force_compact(self: &Arc<Self>) {
        if self.state.lock().unwrap().current_state(MAIN_AGENT_ID) == SessionState::Processing {
            warn!(
                "[{}] force_compact ignored — agent is mid-turn",
                self.instance_id
            );
            return;
        }
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            engine.run_force_compact().await;
        });
    }

    async fn run_force_compact(&self) {
        let messages = {
            let state = self.state.lock().unwrap();
            state.message_history(MAIN_AGENT_ID)
        };
        if messages.len() < 16 {
            info!(
                "[{}] force_compact: history too short ({} msgs)",
                self.instance_id,
                messages.len()
            );
            return;
        }

        let opts = self.options.read().unwrap().clone();
        let profile = self.resolve_model_profile();
        let http_client = self.http_client.clone();
        let event_bus = self.event_bus.clone();
        let permission_manager = self.permission_manager.clone();
        let response_registry = self.response_registry.clone();
        let session_id = self.session_id.read().unwrap().clone().unwrap_or_default();
        let instance_id = self.instance_id.clone();
        let engine_for_tools = self.self_weak.lock().unwrap().clone();
        let tools_resolver: conversation::ToolsResolver = Arc::new(move || {
            engine_for_tools
                .upgrade()
                .map(|e| e.tools_for_main_agent())
                .unwrap_or_default()
        });

        let cancel = CancellationToken::new();
        {
            let mut state = self.state.lock().unwrap();
            state.current_abort = Some(cancel.clone());
        }

        let config = conversation::QueryConfig {
            agent_id: MAIN_AGENT_ID.to_string(),
            chat_jid: instance_id.clone(),
            working_dir: opts.working_dir.clone(),
            agent_data_dir: opts.agent_data_dir.clone(),
            system_prompt: opts.system_prompt.clone(),
            tools: tools_resolver,
            http_client,
            event_bus: event_bus.clone(),
            response_registry: Some(response_registry),
            permission_checker: permission_manager,
            profile,
            thinking: false,
            stream: false,
            is_subagent: false,
            hook_manager: Some(self.hook_manager.clone()),
            hook_client: Some(self.http_client.clone()),
            hook_profile: Some(self.resolve_model_profile()),
            session_id,
            enable_cache: false,
            agent_status: false,
            agent_mode: opts.agent_mode,
            max_turns_override: None,
            pending_inject: None,
        };

        info!("[{instance_id}] force_compact: starting");
        let compacted =
            conversation::compact_now_forced(messages, &config, &cancel).await;
        {
            let mut st = self.state.lock().unwrap();
            st.set_message_history(MAIN_AGENT_ID, compacted.clone());
            st.current_abort = None;
        }
        Self::persist_llm_history(&instance_id, &compacted);
        self.identity_refresh_pending
            .store(true, Ordering::Relaxed);
        info!(
            "[{instance_id}] force_compact: done ({} msgs)",
            compacted.len()
        );
    }

    /// Hot-update the rendered `## User defaults` system-prompt block (set
    /// from the global `defaults` config on each turn). `None` = nothing
    /// configured → the system prompt stays untouched.
    pub fn set_user_defaults(&self, block: Option<String>) {
        self.options.write().unwrap().user_defaults = block;
    }

    /// Hot-update the per-group LLM override for this engine. `id` is an entry id
    /// in the global `llmConfigs` list, or `None` to fall back to the globally
    /// active model. Takes effect on the next turn (the request path re-resolves
    /// the profile via [`Self::resolve_model_profile`]).
    pub fn set_model_override(&self, id: Option<String>) {
        self.options.write().unwrap().model_config_id = id;
    }

    /// Deterministically pick the best-matching skill for a prompt by scoring it
    /// against each skill's `when-to-use` text **and** its explicit `triggers`
    /// list (quoted-phrase + word overlap, no LLM call). Returns the winning
    /// skill name, or `None` if nothing clears the confidence threshold. Skills
    /// with `disable_model_invocation` are excluded.
    fn match_skill_name(&self, prompt: &str) -> Option<String> {
        use crate::skills::matching::{best, score, Reading};
        best(&score(&self.skill_cards(), prompt, Reading::Legacy)).map(|s| s.name.clone())
    }

    fn build_skill_match_reminder(&self, prompt: &str) -> Option<String> {
        let name = self.match_skill_name(prompt)?;
        self.skill_hint(&name)
    }

    /// This turn's skill block, by priority: an explicit `#skill` directive;
    /// then the pre-skill router's decision when it routed this turn (taken
    /// once — load, hint, or nothing); then the legacy keyword matcher, which
    /// loads the match when `preTriggerSkill` is on and hints it otherwise.
    fn skill_block_for_turn(&self, prompt: &str, forced: Option<String>, pre_trigger: bool) -> Option<String> {
        let routed = self.skill_route.lock().unwrap().take();
        if forced.is_some() {
            return forced;
        }
        if let Some(route) = routed {
            return route.and_then(|r| {
                if r.force {
                    self.force_skill_reminder(&r.name)
                } else {
                    self.skill_hint(&r.name)
                }
            });
        }
        pre_trigger
            .then(|| self.match_skill_name(prompt))
            .flatten()
            .and_then(|name| self.force_skill_reminder(&name))
            .or_else(|| self.build_skill_match_reminder(prompt))
    }

    /// A soft "this skill may help" reminder for `name`.
    fn skill_hint(&self, name: &str) -> Option<String> {
        let skill = self.skill_registry.find(name)?;
        let desc = skill.metadata.description;
        let first_sentence = desc.split('.').next().unwrap_or(&desc).trim();
        Some(format!(
            "<system-reminder>\n\
Skill hint: `{name}` may help with this request — {first_sentence}.\n\
\n\
**Workflow:**\n\
1. Invoke it with `Skill {{ \"skill\": \"{name}\" }}` to load its instructions.\n\
2. Follow the skill's workflow if it fits the user's exact request.\n\
3. For time-sensitive or external data, use a live data tool; do not answer from memory.\n\
</system-reminder>\n\n"
        ))
    }

    /// Whether the given instance id corresponds to an agent that operates
    /// inside a real workspace (code edits, file ops, project bash). Only
    /// these sessions get `SENCLAW.md` injected into their first user turn;
    /// regular chat agents (`web:*`, `app:*`, `virtual:*`) skip it to save
    /// ~2k tokens per turn and avoid polluting attention with project docs
    /// irrelevant to the chat query.
    fn instance_uses_workspace(_instance_id: &str) -> bool {
        false
    }

    /// Read `SENCLAW.md` (walks up from `working_dir`), the user profile, and
    /// the current date. Returns a `<system-reminder>` block to inject into the
    /// first user turn, or `None` if nothing useful was found.
    ///
    /// When `include_project_doc` is `false`, only the date line is emitted —
    /// the project markdown is skipped entirely. Used for non-workspace agents.
    ///
    /// Reads `SENCLAW.md` first, then falls back to `CLAUDE.md` for backward
    /// compatibility with existing repos that haven't renamed yet.
    ///
    /// The user profile (Soul Core) rides here rather than in the per-turn
    /// memory blocks in `AgentPool` because it is stable for the whole
    /// session: emitting it once keeps the system prompt cacheable and costs
    /// its tokens a single time instead of every turn. `instance_id` is the
    /// chat JID, which is what decides how much of the profile this context is
    /// allowed to see — see [`crate::user_profile::ProfileScope`].
    fn collect_first_turn_context(
        working_dir: &str,
        include_project_doc: bool,
        instance_id: &str,
    ) -> Option<String> {
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut parts: Vec<String> = vec![format!("Today's date is {date}.")];

        // Soul Core: who the human is. Tier filtering happens inside
        // `block_for_instance`; never inline the profile here.
        let cfg = crate::config::Config::from_env();
        if let Some(block) = crate::user_profile::block_for_instance(&cfg, instance_id) {
            parts.push(block);
        }
        if let Some(block) = crate::user_profile::tools_notes_block(&cfg, instance_id) {
            parts.push(block);
        }

        if include_project_doc {
            // Walk up the directory tree looking for SENCLAW.md, then CLAUDE.md.
            let mut dir = std::path::Path::new(working_dir).to_path_buf();
            let project_doc: Option<(&'static str, String)> = loop {
                let mut hit: Option<(&'static str, String)> = None;
                for fname in ["SENCLAW.md", "CLAUDE.md"] {
                    let candidate = dir.join(fname);
                    if candidate.exists() {
                        if let Ok(content) = std::fs::read_to_string(&candidate) {
                            hit = Some((fname, content));
                            break;
                        }
                    }
                }
                if let Some(found) = hit {
                    break Some(found);
                }
                if !dir.pop() {
                    break None;
                }
            };
            if let Some((fname, content)) = project_doc {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    // Cap the project doc. It was read unbounded, which is a
                    // per-session cost with no ceiling — this repo's own
                    // CLAUDE.md is 34 KB. Char-boundary truncation, because a
                    // byte slice through a multi-byte character panics.
                    let body = crate::util::text::truncate_on_char_boundary(
                        trimmed,
                        crate::user_profile::MAX_FLAT_FILE_CHARS,
                    );
                    parts.push(format!("Project instructions ({fname}):\n{body}"));
                }
            }
        }

        if parts.len() == 1 && parts[0].contains("date") {
            // Only date — still inject so the model knows the day.
            return Some(format!(
                "<system-reminder>\n{}\n</system-reminder>\n\n",
                parts[0]
            ));
        }

        Some(format!(
            "<system-reminder>\n{}\n</system-reminder>\n\n",
            parts.join("\n\n")
        ))
    }

    pub fn initialize_plugins(&self) {
        let opts = self.options.read().unwrap();
        debug!(
            "[{}] initialize_plugins: working_dir={}",
            self.instance_id, opts.working_dir
        );

        // Build a minimal config for skill scanning
        let config = crate::config::Config::from_env();
        let entries = crate::skills::scan::load_all_local_skills(&config);
        debug!(
            "[{}] scanned {} skill entries",
            self.instance_id,
            entries.len()
        );

        if !entries.is_empty() {
            self.skill_registry.load_entries(&entries);
            debug!(
                "[{}] registered {} skills",
                self.instance_id,
                self.skill_registry.len()
            );
        }
    }
}

/// Human title for an MCP tool, shown on permission cards and tool results.
///
/// Built-in tools show their name alone. The server segment was pure repetition
/// for them — the tool name already opens with its domain ("Space Current
/// Time", "Memory Search", "Profile Update"), so the prefix read as "SPACE:
/// Space Current Time". Under bundling it was worse than redundant: every
/// built-in was labelled with the host process ("CORE:", later "CORE:"),
/// naming the transport rather than anything the user recognises.
///
/// External servers keep the prefix — a Space App's tool name carries no hint
/// of which app it came from, and that is worth showing.
fn mcp_display_title(server_name: &str, tool_name: &str) -> String {
    let capitalized = tool_name
        .replace('_', " ")
        .split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    if server_name.starts_with("senclaw-") {
        return capitalized;
    }
    format!("{}: {}", server_name.to_uppercase(), capitalized)
}

// ===== McpRegistryBridgeTool =====
// Wraps a single tool from a spawned MCP subprocess so it can participate in
// the ZenEngine's builtin_tools roster and be visible to the LLM.

struct McpRegistryBridgeTool {
    /// Full tool name sent to the LLM, e.g. `mcp__senclaw-browser__navigate`
    full_name: String,
    /// Short tool name used for subprocess `tools/call`
    tool_name: String,
    /// MCP server name (key in registry)
    server_name: String,
    desc: String,
    schema: Value,
    registry: SharedMcpRegistry,
}

#[async_trait::async_trait]
impl Tool for McpRegistryBridgeTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.desc
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let result = self
            .registry
            .call_tool(&self.server_name, &self.tool_name, input)
            .await?;
        let summary = match &result {
            Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| format!("{other:?}")),
        };
        Ok(vec![ToolOutput::Result {
            data: result,
            result_for_assistant: summary,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, input: &Value) -> ToolResultMessage {
        let summary = match data {
            Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| format!("{other:?}")),
        };
        ToolResultMessage {
            title: self.get_display_title(input),
            summary,
            content: data.clone(),
        }
    }

    fn get_display_title(&self, _input: &Value) -> String {
        mcp_display_title(&self.server_name, &self.tool_name)
    }

    fn gen_tool_permission(&self, input: &Value) -> Option<ToolPermissionInfo> {
        Some(ToolPermissionInfo {
            title: self.get_display_title(input),
            content: input.clone(),
        })
    }

    // ===== Lazy-load policy =====
    //
    // Mirrors `McpBridgeTool::should_defer` (the *other* MCP wrapper used by
    // `refresh_mcp_tools`). Both bridge structs need identical defer policy
    // or some MCP tools will leak into the initial prompt.
    fn should_defer(&self) -> bool {
        !crate::mcp::bridge::ALWAYS_LOADED_MCP_TOOLS.contains(&self.full_name.as_str())
    }

    fn search_hint(&self) -> String {
        // `server_name — tool_name — first sentence of description`
        let first_sentence = self.desc.split('.').next().unwrap_or("").trim().to_string();
        let server_display = self
            .server_name
            .strip_prefix("senclaw-")
            .unwrap_or(&self.server_name);
        if first_sentence.is_empty() {
            format!("{server_display} {tool}", tool = self.tool_name)
        } else {
            format!(
                "{server_display} {tool} — {first_sentence}",
                tool = self.tool_name
            )
        }
    }
}

/// Best-effort detection of the user's language from a message, returning a
/// human-readable name to lock into a per-turn reminder. Returns `None` when
/// the script is ambiguous (plain ASCII) — the system prompt's generic
/// "user's language" rule covers that case.
///
/// Scoped to the languages this deployment actually serves (Vietnamese,
/// Chinese, English); not a general language identifier.
fn detect_user_language(text: &str) -> Option<&'static str> {
    let mut has_cjk = false;
    let mut has_viet = false;
    for c in text.chars() {
        let u = c as u32;
        if (0x4E00..=0x9FFF).contains(&u) || (0x3400..=0x4DBF).contains(&u) {
            has_cjk = true;
        } else if (0x1EA0..=0x1EFF).contains(&u)        // Latin Extended Additional — almost all Vietnamese
            || matches!(u, 0x0110 | 0x0111             // Đ đ
                          | 0x01A0 | 0x01A1            // Ơ ơ
                          | 0x01AF | 0x01B0            // Ư ư
                          | 0x0102 | 0x0103)
        // Ă ă
        {
            has_viet = true;
        }
    }
    if has_viet {
        Some("Vietnamese")
    } else if has_cjk {
        Some("Chinese")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::matching::extract_quoted_phrases;

    #[test]
    fn discovered_has_matches_both_naming_schemes() {
        let mut set = DiscoveredTools::default();
        // The agent-browser load hook inserts the STRIPPED bridge name…
        set.insert("mcp__browser__search".to_string());
        // …yet the deferred tool is registered under its FULL name. Membership
        // must still hold, or the tool never un-defers.
        assert!(discovered_has(&set, "mcp__senclaw-browser__browser_search"));
        assert!(discovered_has(&set, "mcp__browser__search"));
        // ToolSearch / apply_skill_activation insert the FULL name directly.
        let mut set2 = DiscoveredTools::default();
        set2.insert("mcp__senclaw-space__space_note_create".to_string());
        assert!(discovered_has(
            &set2,
            "mcp__senclaw-space__space_note_create"
        ));
        // A genuinely-undiscovered tool stays out.
        assert!(!discovered_has(
            &set,
            "mcp__senclaw-space__space_note_create"
        ));
    }

    #[test]
    fn detect_user_language_covers_vi_zh_en() {
        assert_eq!(
            detect_user_language("tìm kiếm giá vàng hôm nay"),
            Some("Vietnamese")
        );
        assert_eq!(
            detect_user_language("đổi mật khẩu giúp tôi"),
            Some("Vietnamese")
        );
        assert_eq!(detect_user_language("今天黄金价格"), Some("Chinese"));
        // Plain ASCII is ambiguous → defer to the system prompt's generic rule.
        assert_eq!(detect_user_language("what is the gold price today"), None);
    }

    #[test]
    fn extract_quoted_phrases_straight_quotes() {
        let out = extract_quoted_phrases("e.g. \"tìm giá vàng hôm nay\", \"screenshot github\"");
        assert_eq!(out, vec!["tìm giá vàng hôm nay", "screenshot github"]);
    }

    #[test]
    fn extract_quoted_phrases_curly() {
        let out = extract_quoted_phrases("\u{201C}hello world\u{201D}");
        assert_eq!(out, vec!["hello world"]);
    }

    #[test]
    fn extract_quoted_phrases_empty_when_none() {
        assert!(extract_quoted_phrases("no quotes here").is_empty());
    }

    #[test]
    fn engine_creation_and_basic_state() {
        let opts = ZenCoreOptions {
            instance_id: "test-1".into(),
            agent_data_dir: "/tmp/test".into(),
            working_dir: "/tmp/test".into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        assert_eq!(engine.instance_id, "test-1");
    }

    #[test]
    fn create_session_generates_id() {
        let opts = ZenCoreOptions {
            instance_id: "test-2".into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        engine.create_session(None).unwrap();
        let sid = engine.session_id.read().unwrap();
        assert!(sid.is_some());
    }

    #[test]
    fn create_session_with_provided_id() {
        let opts = ZenCoreOptions {
            instance_id: "test-3".into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        engine.create_session(Some("my-session")).unwrap();
        let sid = engine.session_id.read().unwrap();
        assert_eq!(sid.as_deref(), Some("my-session"));
    }

    #[test]
    fn pause_session_sets_state() {
        let opts = ZenCoreOptions {
            instance_id: "test-4".into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        engine.create_session(None).unwrap();
        engine.pause_session();
        let state = engine.state.lock().unwrap();
        assert_eq!(state.current_state(MAIN_AGENT_ID), SessionState::Paused);
    }

    #[test]
    fn has_session_tool_results_false_on_empty() {
        let opts = ZenCoreOptions {
            instance_id: "test-5".into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        assert!(!engine.has_session_tool_results());
    }

    #[test]
    fn update_skip_permissions_toggles_all() {
        let opts = ZenCoreOptions {
            instance_id: "test-6".into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        engine.update_skip_permissions(true);
        let o = engine.options.read().unwrap();
        assert!(o.skip_file_edit_permission);
        assert!(o.skip_bash_exec_permission);
        assert!(o.skip_skill_permission);
    }

    #[test]
    fn tools_for_main_agent_respects_use_tools_whitelist() {
        let opts = ZenCoreOptions {
            instance_id: "test-tools-1".into(),
            use_tools: vec!["Bash".into(), "Read".into()],
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let tools = engine.tools_for_main_agent();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"Bash"));
        assert!(names.contains(&"Read"));
        assert!(!names.contains(&"Write"));
        assert!(!names.contains(&"Glob"));
    }

    #[test]
    fn use_tools_whitelist_never_strips_toolsearch() {
        // A group `allowed_tools` whitelist that forgets to list `ToolSearch`
        // must NOT strip it: it is `always_load()`, the agent's only path to
        // load any deferred tool. Stripping it strands the agent with
        // "No such tool available: ToolSearch".
        let opts = ZenCoreOptions {
            instance_id: "test-tools-keepsearch".into(),
            use_tools: vec!["Bash".into()],
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let tools = engine.tools_for_main_agent();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"Bash"));
        assert!(
            names.contains(&"ToolSearch"),
            "ToolSearch must survive a use_tools whitelist that omits it"
        );
    }

    #[test]
    fn tools_for_main_agent_empty_use_tools_returns_all() {
        let opts = ZenCoreOptions {
            instance_id: "test-tools-2".into(),
            use_tools: vec![],
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let tools = engine.tools_for_main_agent();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"Bash"));
        assert!(names.contains(&"TodoWrite"));
    }

    #[test]
    fn task_done_only_in_plan_and_dag_modes() {
        use crate::tools::TASK_DONE_TOOL_NAME;
        // Agent mode: task_done must NOT be offered (no enforced submit step).
        let agent = ZenEngine::new(
            ZenCoreOptions {
                instance_id: "td-agent".into(),
                agent_mode: AgentMode::Agent,
                ..Default::default()
            },
            None,
        );
        let agent_tools = agent.tools_for_main_agent();
        assert!(
            !agent_tools.iter().any(|t| t.name() == TASK_DONE_TOOL_NAME),
            "task_done must be stripped in Agent mode"
        );

        // Plan mode: task_done is the abandon/completion signal — keep it.
        let plan = ZenEngine::new(
            ZenCoreOptions {
                instance_id: "td-plan".into(),
                agent_mode: AgentMode::Plan,
                ..Default::default()
            },
            None,
        );
        let plan_tools = plan.tools_for_main_agent();
        assert!(
            plan_tools.iter().any(|t| t.name() == TASK_DONE_TOOL_NAME),
            "task_done must be available in Plan mode"
        );

        // Dag mode: task_done is the trivial-completion signal — keep it.
        let dag = ZenEngine::new(
            ZenCoreOptions {
                instance_id: "td-dag".into(),
                agent_mode: AgentMode::Dag,
                ..Default::default()
            },
            None,
        );
        let dag_tools = dag.tools_for_main_agent();
        assert!(
            dag_tools.iter().any(|t| t.name() == TASK_DONE_TOOL_NAME),
            "task_done must be available in Dag mode"
        );
    }

    #[test]
    fn tools_for_main_agent_plan_mode_drops_todo_write() {
        let opts = ZenCoreOptions {
            instance_id: "test-tools-3".into(),
            agent_mode: AgentMode::Plan,
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let tools = engine.tools_for_main_agent();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(!names.contains(&"TodoWrite"));
    }

    #[test]
    fn tools_for_main_agent_plan_mode_strips_write_tools_keeps_readonly() {
        let opts = ZenCoreOptions {
            instance_id: "test-plan-enforce".into(),
            agent_mode: AgentMode::Plan,
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let names: Vec<String> = engine
            .tools_for_main_agent()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        let has = |n: &str| names.iter().any(|x| x == n);
        // Mutating tools are physically stripped in plan mode.
        for write_tool in ["Write", "Edit", "NotebookEdit", "Bash", "TodoWrite"] {
            assert!(!has(write_tool), "plan mode must strip {write_tool}");
        }
        // Read-only research tools survive.
        for ro in ["Read", "Grep", "Glob"] {
            assert!(has(ro), "plan mode must keep {ro}");
        }
        // ExitPlanMode (non-read-only escape hatch) survives.
        assert!(has("ExitPlanMode"), "plan mode must keep ExitPlanMode");
    }

    #[test]
    fn agent_mode_keeps_write_tools() {
        // Sanity: in Agent mode, write tools are present (the plan-mode
        // strip is mode-gated, not global).
        let opts = ZenCoreOptions {
            instance_id: "test-agent-mode".into(),
            agent_mode: AgentMode::Agent,
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let names: Vec<String> = engine
            .tools_for_main_agent()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "Write"));
        assert!(names.iter().any(|n| n == "Bash"));
    }

    #[test]
    fn respond_to_plan_exit_unblocks_tool_and_flips_mode() {
        // Engine starts in Plan mode. A waiter registered under the agent_id
        // (as ExitPlanMode does) must receive the "selected" answer, and the
        // mode must flip back to Agent on approval.
        let opts = ZenCoreOptions {
            instance_id: "test-plan-exit".into(),
            agent_mode: AgentMode::Plan,
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let mut rx = engine.response_registry.register_ask_question("agent-x");

        engine.respond_to_plan_exit(PlanExitResponseData {
            agent_id: "agent-x".into(),
            selected: "startEditing".into(),
        });

        // The waiter got the choice.
        let answer = rx
            .try_recv()
            .expect("plan-exit response delivered to waiter");
        assert_eq!(
            answer.answers.get("selected").map(String::as_str),
            Some("startEditing")
        );
        // Mode flipped back to Agent.
        assert_eq!(engine.options.read().unwrap().agent_mode, AgentMode::Agent);
    }

    #[test]
    fn respond_to_plan_exit_cancel_keeps_plan_mode() {
        let opts = ZenCoreOptions {
            instance_id: "test-plan-cancel".into(),
            agent_mode: AgentMode::Plan,
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let mut rx = engine.response_registry.register_ask_question("agent-y");

        engine.respond_to_plan_exit(PlanExitResponseData {
            agent_id: "agent-y".into(),
            selected: "cancelled".into(),
        });

        let answer = rx.try_recv().expect("cancel still delivered");
        assert_eq!(
            answer.answers.get("selected").map(String::as_str),
            Some("cancelled")
        );
        // Cancel → stays in Plan mode.
        assert_eq!(engine.options.read().unwrap().agent_mode, AgentMode::Plan);
    }

    #[test]
    fn tools_for_subagent_strips_excluded_set() {
        let opts = ZenCoreOptions {
            instance_id: "test-sub-1".into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let tools = engine.tools_for_subagent(None);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        // SUBAGENT_EXCLUDED_TOOLS items must be gone.
        for excluded in ["Task", "TodoWrite", "PeekBgJob", "ExitPlanMode"] {
            assert!(!names.contains(&excluded), "should drop {excluded}");
        }
        // Read/Bash should still be there.
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Bash"));
    }

    #[test]
    fn tools_for_subagent_respects_agent_whitelist() {
        let opts = ZenCoreOptions {
            instance_id: "test-sub-2".into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let allowed = vec!["Read".to_string(), "Glob".to_string()];
        let tools = engine.tools_for_subagent(Some(&allowed));
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names
                .iter()
                .filter(|n| **n == "Read" || **n == "Glob")
                .count(),
            2
        );
        assert!(!names.contains(&"Bash"));
        assert!(!names.contains(&"Write"));
    }

    #[test]
    fn tools_for_subagent_star_inherits_full_main_minus_excluded() {
        let opts = ZenCoreOptions {
            instance_id: "test-sub-3".into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let star = vec!["*".to_string()];
        let tools_star = engine.tools_for_subagent(Some(&star));
        let names_star: std::collections::HashSet<&str> =
            tools_star.iter().map(|t| t.name()).collect();
        let tools_none = engine.tools_for_subagent(None);
        let names_none: std::collections::HashSet<&str> =
            tools_none.iter().map(|t| t.name()).collect();
        assert_eq!(names_star, names_none);
    }

    #[test]
    fn tools_for_subagent_inherits_use_tools_filter() {
        let opts = ZenCoreOptions {
            instance_id: "test-sub-4".into(),
            use_tools: vec!["Bash".into(), "Read".into(), "Task".into()],
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        let tools = engine.tools_for_subagent(None);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        // Bash + Read survive use_tools filter. Task gets stripped by SUBAGENT_EXCLUDED_TOOLS.
        assert!(names.contains(&"Bash"));
        assert!(names.contains(&"Read"));
        assert!(!names.contains(&"Task"));
        assert!(!names.contains(&"Write")); // not in use_tools
    }

    #[test]
    fn cap_git_status_passes_short_through() {
        let gs = "## main\n M a.rs\n M b.rs";
        assert_eq!(ZenEngine::cap_git_status(gs), gs);
    }

    #[test]
    fn cap_git_status_truncates_long() {
        let mut s = String::from("## main");
        for i in 0..100 {
            s.push_str(&format!("\n M file{i}.rs"));
        }
        let out = ZenEngine::cap_git_status(&s);
        assert_eq!(out.lines().count(), 31); // 30 kept + 1 summary line
        assert!(out.contains("## main")); // branch header preserved
        assert!(out.contains("71 more changed path(s) omitted"));
    }

    fn engine_with_skill(instance: &str, skill_name: &str, body: &str) -> Arc<ZenEngine> {
        use crate::skills::metadata::SkillMetadata;
        use crate::skills::scan::SkillEntry;

        let dir = std::env::temp_dir().join(format!(
            "senclaw_skilltest_{}_{}_{}",
            std::process::id(),
            instance,
            skill_name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("SKILL.md");
        std::fs::write(
            &file_path,
            format!("---\nname: {skill_name}\ndescription: test\n---\n\n{body}\n"),
        )
        .unwrap();

        let opts = ZenCoreOptions {
            instance_id: instance.into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        engine.skill_registry.load_entries(&[SkillEntry {
            name: skill_name.into(),
            description: "test".into(),
            version: None,
            source: "bundled".into(),
            dir: dir.clone(),
            file_path,
            metadata: SkillMetadata {
                name: skill_name.into(),
                description: "test".into(),
                ..Default::default()
            },
            eligible: true,
            ineligible_reason: None,
        }]);
        engine
    }

    #[test]
    fn detect_explicit_skill_matches_hash_and_slash_and_separator_variants() {
        let engine = engine_with_skill("test-skill-1", "ssh-connect", "Use ssh_list_hosts.");
        // `#`, `/`, hyphen/underscore, and case variants all resolve.
        for raw in [
            "#ssh-connect kết nối host test",
            "/ssh-connect kết nối host test",
            "#ssh_connect kết nối host test",
            "#SSH-Connect kết nối host test",
        ] {
            let (name, rest) = engine
                .detect_explicit_skill(raw)
                .unwrap_or_else(|| panic!("should detect in {raw:?}"));
            assert_eq!(name, "ssh-connect");
            assert_eq!(rest, "kết nối host test");
        }
        // A bare directive with no trailing request still resolves.
        let (name, rest) = engine.detect_explicit_skill("#ssh-connect").unwrap();
        assert_eq!(name, "ssh-connect");
        assert!(rest.is_empty());
        // Ordinary text that merely starts with `/` or `#` is left alone.
        assert!(engine
            .detect_explicit_skill("/Users/benji/file.rs")
            .is_none());
        assert!(engine
            .detect_explicit_skill("#hashtag not a skill")
            .is_none());
        assert!(engine.detect_explicit_skill("just connect ssh").is_none());
    }

    /// Build an engine with one skill carrying explicit `when_to_use` + `triggers`
    /// metadata, for exercising the deterministic pre-trigger matcher.
    fn engine_with_skill_meta(
        instance: &str,
        skill_name: &str,
        when_to_use: Option<&str>,
        triggers: &[&str],
    ) -> Arc<ZenEngine> {
        use crate::skills::metadata::SkillMetadata;
        use crate::skills::scan::SkillEntry;

        let dir = std::env::temp_dir().join(format!(
            "senclaw_skillmeta_{}_{}_{}",
            std::process::id(),
            instance,
            skill_name
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("SKILL.md");
        std::fs::write(&file_path, "---\nname: x\ndescription: test\n---\n\nbody\n").unwrap();

        let engine = ZenEngine::new(
            ZenCoreOptions {
                instance_id: instance.into(),
                ..Default::default()
            },
            None,
        );
        engine.skill_registry.load_entries(&[SkillEntry {
            name: skill_name.into(),
            description: "Drive the browser to find live web content.".into(),
            version: None,
            source: "bundled".into(),
            dir: dir.clone(),
            file_path,
            metadata: SkillMetadata {
                name: skill_name.into(),
                description: "Drive the browser to find live web content.".into(),
                when_to_use: when_to_use.map(|s| s.to_string()),
                triggers: triggers.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            eligible: true,
            ineligible_reason: None,
        }]);
        engine
    }

    #[test]
    fn match_skill_name_scores_when_to_use_and_triggers() {
        // Matches via when-to-use word overlap (needs >=15: 3 words x 5).
        let e = engine_with_skill_meta(
            "m1",
            "agent-browser",
            Some("search web browser for current prices and news"),
            &[],
        );
        assert_eq!(
            e.match_skill_name("please search the web browser for prices")
                .as_deref(),
            Some("agent-browser")
        );
        // Unrelated prompt does not match.
        assert_eq!(e.match_skill_name("hello there friend").as_deref(), None);

        // Matches via an explicit multi-word trigger phrase (+50, clears bar).
        let e2 = engine_with_skill_meta("m2", "agent-browser", None, &["giá vàng hôm nay"]);
        assert_eq!(
            e2.match_skill_name("cho tôi giá vàng hôm nay").as_deref(),
            Some("agent-browser")
        );
        assert_eq!(e2.match_skill_name("what time is it").as_deref(), None);
    }

    #[test]
    fn a_routed_turn_uses_the_routers_decision_once() {
        use crate::skills::matching::SkillRoute;
        let e = engine_with_skill_meta("route", "agent-browser", None, &["giá vàng hôm nay"]);
        let prompt = "cho tôi giá vàng hôm nay";
        // Legacy: preTriggerSkill loads the keyword match.
        assert!(e.skill_block_for_turn(prompt, None, true).unwrap().contains("explicitly invoked"));
        // The router said "hint": a hint, even with preTriggerSkill on…
        e.set_skill_route(Some(SkillRoute { name: "agent-browser".into(), force: false }));
        assert!(e.skill_block_for_turn(prompt, None, true).unwrap().contains("Skill hint"));
        // …once: the next turn is the legacy matcher's again.
        assert!(e.skill_block_for_turn(prompt, None, true).unwrap().contains("explicitly invoked"));
        // The router saw no skill for it: nothing, although the keywords match.
        e.set_skill_route(None);
        assert_eq!(e.skill_block_for_turn(prompt, None, true), None);
        // An explicit directive still wins over the router (and clears it).
        e.set_skill_route(None);
        assert_eq!(e.skill_block_for_turn(prompt, Some("forced".into()), true).as_deref(), Some("forced"));
        assert!(e.skill_block_for_turn(prompt, None, false).unwrap().contains("Skill hint"));
    }

    #[test]
    fn set_pre_trigger_skill_flips_option() {
        let e = engine_with_skill_meta("m3", "s", None, &[]);
        assert!(!e.options.read().unwrap().pre_trigger_skill);
        e.set_pre_trigger_skill(true);
        assert!(e.options.read().unwrap().pre_trigger_skill);
    }

    #[test]
    fn user_defaults_block_lands_in_system_prompt() {
        let e = engine_with_skill_meta("m3b", "s", None, &[]);
        assert!(e.options.read().unwrap().user_defaults.is_none());
        e.set_user_defaults(Some("## User defaults\n- Search: use x".into()));
        assert_eq!(
            e.options.read().unwrap().user_defaults.as_deref(),
            Some("## User defaults\n- Search: use x")
        );
        // And the assembled prompt actually carries it (None → untouched).
        let with = ZenEngine::assemble_system_prompt(
            "base",
            "/tmp",
            None,
            None,
            None,
            None,
            None,
            Some("## User defaults\n- Search: use x"),
            None,
            None,
            None,
            None,
        );
        assert!(with.contains("## User defaults"));
        let without = ZenEngine::assemble_system_prompt(
            "base", "/tmp", None, None, None, None, None, None, None, None, None, None,
        );
        assert!(!without.contains("## User defaults"));
    }

    #[test]
    fn builtin_tool_titles_carry_no_server_prefix() {
        // The card used to read "CORE: Space Current Time" — the host process
        // name, which means nothing to the reader, in front of a tool name
        // that already says "Space".
        assert_eq!(
            mcp_display_title("senclaw-core", "space_current_time"),
            "Space Current Time"
        );
        assert_eq!(
            mcp_display_title("senclaw-profile", "profile_update"),
            "Profile Update"
        );
        for server in ["senclaw-core", "senclaw-memory", "senclaw-space"] {
            let title = mcp_display_title(server, "space_event_create");
            assert!(!title.contains(':'), "{server} still prefixes: {title}");
        }
    }

    #[test]
    fn external_tool_titles_keep_their_server() {
        // A Space App's tool name says nothing about which app it belongs to,
        // so dropping the prefix there would lose real information.
        assert_eq!(
            mcp_display_title("ssh-manager-mcp", "ssh_execute_command"),
            "SSH-MANAGER-MCP: Ssh Execute Command"
        );
    }

    #[test]
    fn soul_block_lands_in_system_prompt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "# Identity\nYou are Sen.\n").unwrap();
        let soul = ZenEngine::load_soul_prompt_block(dir.path().to_str().unwrap()).unwrap();
        assert!(soul.contains("# Persona (SOUL.md)"));
        assert!(soul.contains("You are Sen."));
        let out = ZenEngine::assemble_system_prompt(
            "BASE",
            "/tmp",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&soul),
        );
        let base_at = out.find("BASE").unwrap();
        let soul_at = out.find("Persona (SOUL.md)").unwrap();
        assert!(base_at < soul_at);
    }

    #[test]
    fn operating_rules_land_after_the_base_prompt() {
        // Order is a security property, not formatting: AGENTS.md is text the
        // user types, so it must never precede the base prompt's safety
        // section.
        let out = ZenEngine::assemble_system_prompt(
            "BASE_MARKER",
            "/tmp",
            None,
            None,
            None,
            None,
            None,
            None,
            Some("<user_operating_rules>\nRULE_MARKER\n</user_operating_rules>"),
            None,
            None,
            None,
        );
        let base_at = out.find("BASE_MARKER").expect("base present");
        let rule_at = out.find("RULE_MARKER").expect("rules present");
        assert!(
            base_at < rule_at,
            "operating rules preceded the base prompt"
        );
    }

    #[test]
    fn operating_rules_absent_leaves_prompt_untouched() {
        let without = ZenEngine::assemble_system_prompt(
            "base", "/tmp", None, None, None, None, None, None, None, None, None, None,
        );
        assert!(!without.contains("user_operating_rules"));
    }

    /// Build an engine from a fully-specified `SkillMetadata` + body.
    fn engine_with_full_skill(
        instance: &str,
        meta: crate::skills::SkillMetadata,
        body: &str,
    ) -> Arc<ZenEngine> {
        use crate::skills::scan::SkillEntry;
        let dir = std::env::temp_dir().join(format!(
            "senclaw_fullskill_{}_{}",
            std::process::id(),
            instance
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("SKILL.md");
        std::fs::write(
            &file_path,
            format!("---\nname: {}\ndescription: d\n---\n\n{body}\n", meta.name),
        )
        .unwrap();
        let engine = ZenEngine::new(
            ZenCoreOptions {
                instance_id: instance.into(),
                ..Default::default()
            },
            None,
        );
        engine.skill_registry.load_entries(&[SkillEntry {
            name: meta.name.clone(),
            description: meta.description.clone(),
            version: None,
            source: "bundled".into(),
            dir: dir.clone(),
            file_path,
            metadata: meta,
            eligible: true,
            ineligible_reason: None,
        }]);
        engine
    }

    #[test]
    fn always_skills_block_injects_full_content_and_params() {
        use crate::skills::{SkillMetadata, SkillParam, SkillUseMode};
        let meta = SkillMetadata {
            name: "house-style".into(),
            description: "House writing style.".into(),
            use_mode: SkillUseMode::Always,
            params: vec![SkillParam {
                name: "tone".into(),
                type_: "string".into(),
                required: true,
                description: Some("formal or casual".into()),
            }],
            ..Default::default()
        };
        let e = engine_with_full_skill("always1", meta, "ALWAYS-BODY-MARKER");
        let block = e.build_always_skills_block().expect("always block present");
        assert!(block.contains("ALWAYS-BODY-MARKER"), "full body injected");
        assert!(block.contains("SKILL: house-style"));
        assert!(block.contains("Parameters this skill accepts"));
        assert!(block.contains("`tone`"));

        // A trigger-mode skill produces no always block.
        let meta2 = SkillMetadata {
            name: "trig".into(),
            use_mode: SkillUseMode::Trigger,
            ..Default::default()
        };
        let e2 = engine_with_full_skill("always2", meta2, "TRIGGER-BODY");
        assert!(e2.build_always_skills_block().is_none());
    }

    #[test]
    fn force_skill_reminder_surfaces_params() {
        use crate::skills::{SkillMetadata, SkillParam, SkillUseMode};
        let meta = SkillMetadata {
            name: "weather".into(),
            description: "Weather lookups.".into(),
            use_mode: SkillUseMode::Trigger,
            params: vec![SkillParam {
                name: "city".into(),
                type_: "string".into(),
                required: true,
                description: None,
            }],
            ..Default::default()
        };
        let e = engine_with_full_skill("force1", meta, "WEATHER-BODY");
        let reminder = e.force_skill_reminder("weather").expect("reminder");
        assert!(reminder.contains("Parameters this skill accepts"));
        assert!(reminder.contains("`city`"));
        assert!(reminder.contains("WEATHER-BODY"));
    }

    /// A deferred MCP tool that is never called.
    struct DeferredStub(&'static str);

    #[async_trait::async_trait]
    impl Tool for DeferredStub {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn call(&self, _input: serde_json::Value, _ctx: &ToolContext<'_>) -> anyhow::Result<Vec<ToolOutput>> {
            Ok(vec![])
        }
        fn gen_tool_result_message(&self, _d: &serde_json::Value, _i: &serde_json::Value) -> ToolResultMessage {
            ToolResultMessage {
                title: String::new(),
                summary: String::new(),
                content: serde_json::Value::Null,
            }
        }
        fn get_display_title(&self, _i: &serde_json::Value) -> String {
            self.0.to_string()
        }
        fn should_defer(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_skill_does_not_load_tools_whose_short_verb_its_prose_happens_to_use() {
        // "status", "get" and "list" are everyday words in a skill's text;
        // matching them loaded every such tool on the machine.
        let e = engine_with_skill(
            "activation-generic-verbs",
            "deploy",
            "Check the status, get the logs, list the hosts, then call space_recurring_create.",
        );
        for name in [
            "mcp__core__status",
            "mcp__ai-office-mcp__get",
            "mcp__core__space_recurring_create",
            "mcp__core__space_recurring_create_all",
        ] {
            e.register_tool(Arc::new(DeferredStub(name)));
        }
        let skill = e.skill_registry.find("deploy").unwrap();
        e.apply_skill_activation(&skill);
        let discovered = e.discovered_tools.lock().unwrap();
        assert!(discovered.contains("mcp__core__space_recurring_create"), "{discovered:?}");
        assert!(!discovered.contains("mcp__core__space_recurring_create_all"), "{discovered:?}");
        assert!(!discovered.contains("mcp__core__status"), "{discovered:?}");
        assert!(!discovered.contains("mcp__ai-office-mcp__get"), "{discovered:?}");
    }

    #[test]
    fn a_skill_still_loads_a_one_word_tool_it_names_in_full() {
        let e = engine_with_skill("activation-full-name", "office", "Use mcp__core__status first.");
        e.register_tool(Arc::new(DeferredStub("mcp__core__status")));
        let skill = e.skill_registry.find("office").unwrap();
        e.apply_skill_activation(&skill);
        assert!(e.discovered_tools.lock().unwrap().contains("mcp__core__status"));
    }

    #[test]
    fn discovered_tools_keep_the_most_recent_within_the_cap() {
        let mut d = DiscoveredTools::default();
        d.pin("DispatchTask".to_string());
        for i in 0..MAX_DISCOVERED_TOOLS {
            assert!(d.insert(format!("tool_{i}")));
        }
        // Re-discovering the oldest makes it the newest instead of a duplicate.
        assert!(!d.insert("tool_0".to_string()));
        assert!(d.insert("one_more".to_string()));
        assert!(d.contains("tool_0"), "refreshed, so kept");
        assert!(!d.contains("tool_1"), "least recently discovered is dropped first");
        assert!(d.contains("one_more"));
        assert!(d.contains("DispatchTask"), "configured tools are never dropped");
        assert_eq!(d.recent.len(), MAX_DISCOVERED_TOOLS);
    }

    #[test]
    fn identifiers_match_whole_words_only() {
        assert!(mentions_identifier("call `space_list` now", "space_list"));
        assert!(!mentions_identifier("call space_list_all now", "space_list"));
        assert!(!mentions_identifier("the budget", "get"));
        assert!(mentions_identifier("mcp__core__status.", "mcp__core__status"));
    }

    #[test]
    fn a_skill_loads_the_tools_it_names_in_the_other_layout() {
        // Bundled install, skill written for the per-server names: the verb
        // `space_note_create` appears nowhere in `mcp__space__note_create`.
        let e = engine_with_skill(
            "activation-layout",
            "note",
            "Before a note, `ToolSearch { query: \"select:mcp__space__note_create\" }`.",
        );
        for name in ["mcp__core__space_note_create", "mcp__core__wiki_write"] {
            e.register_tool(Arc::new(DeferredStub(name)));
        }
        let skill = e.skill_registry.find("note").unwrap();
        e.apply_skill_activation(&skill);
        let discovered = e.discovered_tools.lock().unwrap();
        assert!(discovered.contains("mcp__core__space_note_create"), "{discovered:?}");
        assert!(!discovered.contains("mcp__core__wiki_write"), "{discovered:?}");
    }

    #[test]
    fn force_skill_reminder_inlines_content() {
        let engine = engine_with_skill("test-skill-2", "ssh-connect", "Call ssh_list_hosts first.");
        let reminder = engine.force_skill_reminder("ssh-connect").unwrap();
        assert!(reminder.contains("explicitly invoked the `ssh-connect` skill"));
        assert!(reminder.contains("Call ssh_list_hosts first."));
    }

    #[tokio::test]
    async fn process_user_input_transitions_to_processing() {
        let opts = ZenCoreOptions {
            instance_id: "test-7".into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        engine.create_session(None).unwrap();
        engine.process_user_input("hello", None).unwrap();
        let state = engine.state.lock().unwrap();
        assert_eq!(state.current_state(MAIN_AGENT_ID), SessionState::Processing);
    }

    #[tokio::test]
    async fn input_queues_while_processing() {
        let opts = ZenCoreOptions {
            instance_id: "test-queue".into(),
            ..Default::default()
        };
        let engine = ZenEngine::new(opts, None);
        engine.create_session(None).unwrap();

        let mut rx = engine.event_bus.subscribe();
        engine.process_user_input("first", None).unwrap();
        // Main agent is Processing — these must queue, not start new loops.
        engine.process_user_input("second", None).unwrap();
        engine.process_user_input("/compact", None).unwrap();
        engine.process_user_input("third", None).unwrap();

        {
            let mut state = engine.state.lock().unwrap();
            assert_eq!(state.pending_inputs_len(), 3);
            // Inject items drain up to (not including) the command.
            let injected = state.consume_inject_inputs_before_next_command();
            assert_eq!(injected.len(), 1);
            assert_eq!(injected[0].input, "second");
            assert_eq!(state.pending_inputs_len(), 2);
        }

        // InputReceived events: first is not queued, the rest are.
        let mut received = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let EngineEvent::InputReceived(d) = ev {
                received.push(d);
            }
        }
        assert_eq!(received.len(), 4);
        assert!(!received[0].queued);
        assert!(received[1].queued && received[1].inject);
        assert!(
            received[2].queued && !received[2].inject,
            "/compact is a command"
        );
        assert!(received[3].queued && received[3].inject);
    }

    fn llm_cfg(id: &str, model: &str) -> crate::gateway::group_manager::LlmConfig {
        crate::gateway::group_manager::LlmConfig {
            id: id.into(),
            label: format!("label-{id}"),
            provider: "openai".into(),
            base_url: "https://example/v1".into(),
            api_key: "k".into(),
            model_name: model.into(),
            adapt: "openai".into(),
            max_tokens: 4096,
            context_length: 128000,
            vision: None,
            ..Default::default()
        }
    }

    #[test]
    fn select_llm_config_prefers_override_then_active() {
        let loaded = crate::gateway::group_manager::LlmConfigResult {
            configs: vec![llm_cfg("a", "gpt-a"), llm_cfg("b", "gpt-b")],
            active_id: Some("a".into()),
            active_quick_id: None,
            active_cognitive_id: None,
        };

        // Per-group override wins over the global active id.
        let sel = ZenEngine::select_llm_config(&loaded, Some("b")).unwrap();
        assert_eq!(sel.model_name, "gpt-b");

        // No override → falls back to the global active id.
        let sel = ZenEngine::select_llm_config(&loaded, None).unwrap();
        assert_eq!(sel.model_name, "gpt-a");

        // Unknown override id → fall back to active id (not None).
        let sel = ZenEngine::select_llm_config(&loaded, Some("missing")).unwrap();
        assert_eq!(sel.model_name, "gpt-a");

        // Neither override nor active matches → None (caller uses first config).
        let loaded2 = crate::gateway::group_manager::LlmConfigResult {
            configs: vec![llm_cfg("a", "gpt-a")],
            active_id: Some("nope".into()),
            active_quick_id: None,
            active_cognitive_id: None,
        };
        assert!(ZenEngine::select_llm_config(&loaded2, Some("also-missing")).is_none());
    }

    /// Write `configs` to a throwaway config.json and hand back its path plus
    /// the tempdir guard (drop it and the file goes).
    fn config_file(
        configs: &[crate::gateway::group_manager::LlmConfig],
        active: Option<&str>,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        for c in configs {
            crate::gateway::group_manager::save_llm_config(&path, c).unwrap();
        }
        crate::gateway::group_manager::set_active_llm_config(&path, active).unwrap();
        (dir, path)
    }

    #[test]
    fn model_accepts_images_infers_from_the_model_name() {
        let (_dir, path) = config_file(
            &[llm_cfg("a", "gpt-4o"), llm_cfg("b", "deepseek-chat")],
            Some("a"),
        );
        assert!(ZenEngine::model_accepts_images(&path, None));
        assert!(!ZenEngine::model_accepts_images(&path, Some("b")));
    }

    #[test]
    fn model_accepts_images_honours_the_explicit_flag() {
        // A self-hosted VL model behind a name no pattern can know, and a
        // vision-named model the user switched off because their gateway chokes.
        let mut on = llm_cfg("a", "internal-build-7");
        on.vision = Some(true);
        let mut off = llm_cfg("b", "gpt-4o");
        off.vision = Some(false);
        let (_dir, path) = config_file(&[on, off], Some("b"));

        assert!(ZenEngine::model_accepts_images(&path, Some("a")));
        assert!(!ZenEngine::model_accepts_images(&path, None));
    }

    #[test]
    fn model_accepts_images_follows_the_per_group_override() {
        // The bug this guards: answering "can it see?" for the globally-active
        // model while the turn actually runs on the group's pinned one.
        let (_dir, path) = config_file(
            &[
                llm_cfg("a", "deepseek-chat"),
                llm_cfg("b", "claude-sonnet-4-5"),
            ],
            Some("a"),
        );
        assert!(!ZenEngine::model_accepts_images(&path, None));
        assert!(ZenEngine::model_accepts_images(&path, Some("b")));
    }

    #[test]
    fn model_accepts_images_matches_the_resolvers_own_fallbacks() {
        // A stale group override must answer for the model the turn will really
        // use (the active one), not fail closed — otherwise a vision chat
        // silently drops to OCR after its pinned config is deleted.
        let (_dir, path) = config_file(
            &[llm_cfg("a", "gpt-4o"), llm_cfg("b", "deepseek-chat")],
            Some("a"),
        );
        assert!(ZenEngine::model_accepts_images(&path, Some("deleted")));

        // Neither override nor active resolves → first config, same as
        // resolve_model_profile_at.
        let (_dir2, path2) = config_file(&[llm_cfg("a", "gpt-4o")], Some("gone"));
        assert!(ZenEngine::model_accepts_images(&path2, Some("also-gone")));
    }
}

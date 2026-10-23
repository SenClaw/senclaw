//! Permission manager — gates tool execution behind user approval.
//!
//! Four tool categories (mirrors TS `PermissionManager.ts`):
//! 1. **File edit** tools (Write, Edit, NotebookEdit)
//! 2. **Bash** tool — safe-command whitelist + prefix allowlisting
//! 3. **Skill** tool — per-skill allowlisting
//! 4. **MCP** tools — per-tool allowlisting
//!
//! Non-readonly tools that don't match any skip rule or allowlist emit
//! `tool:permission:request` and suspend until a response arrives via the
//! [`ResponseRegistry`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::events::ResponseRegistry;
use super::run_tools::PermissionChecker;
use super::*;
use crate::util::command_risk::{self, Risk, Scope};

/// How many calls `auto` mode let through, and how many it still asked about,
/// since the daemon started. Read at `GET /api/code/autonomy`: the numbers
/// that say whether the setting does what it promises.
static AUTO_RAN: AtomicU64 = AtomicU64::new(0);
static AUTO_ASKED: AtomicU64 = AtomicU64::new(0);

pub fn autonomy_stats_json() -> serde_json::Value {
    serde_json::json!({
        "ranUnattended": AUTO_RAN.load(AtomicOrdering::Relaxed),
        "askedAnyway": AUTO_ASKED.load(AtomicOrdering::Relaxed),
    })
}

// ============================================================================
// Constants (mirrors TS)
// ============================================================================

/// Tool names that are treated as file-editing tools.
const FILE_EDIT_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit"];

/// Skill tool name.
const SKILL_TOOL_NAME: &str = "Skill";

/// MCP tool prefix.
const MCP_TOOL_PREFIX: &str = "mcp__";

/// MCP tools whose every call is the person's own decision, so no "never ask
/// again" is offered and none saved earlier is honoured: `browser_approve`
/// carries the person's yes to a purchase, a send or a delete the browser
/// engine paused on (its risk tiers), and a standing grant would let the
/// agent answer for them.
const PER_CALL_MCP_TOOLS: &[&str] = &["browser_approve"];

// ============================================================================
// Permission manager
// ============================================================================

/// What `auto` mode decided about one call.
enum AutoDecision {
    /// Not in `auto`, or not a call it grades: the ordinary rules decide.
    NotApplicable,
    /// Safe to run without asking.
    Run,
    /// Ask, and say why.
    Ask(String),
}

pub struct PermissionManager {
    /// Skip flags from engine options. Atomic so both the engine constructor
    /// and the runtime settings toggle can update them through the shared
    /// `Arc<PermissionManager>`. (They were plain bools that were NEVER wired
    /// from `ZenCoreOptions` — unattended one-shot sessions then hung on
    /// permission prompts nobody answers until the step timeout.)
    skip_file_edit: std::sync::atomic::AtomicBool,
    skip_bash: std::sync::atomic::AtomicBool,
    skip_skill: std::sync::atomic::AtomicBool,
    skip_mcp: std::sync::atomic::AtomicBool,
    /// Per-project allowed tools list (tool_name or "Bash(cmd)" keys).
    allowed_tools: Mutex<HashSet<String>>,
    /// Whether global edit permission has been granted this session.
    global_edit_granted: Mutex<bool>,
    /// One-shot response channels (shared with engine).
    response_registry: Arc<ResponseRegistry>,
    /// Event emitter.
    event_bus: EventBus,
    /// [`Autonomy`] as a byte (0 ask, 1 auto, 2 full); atomic so the pool can
    /// change it under a live engine.
    autonomy: AtomicU8,
    /// The chat is a code session; see [`Self::reversible`].
    code_session: AtomicBool,
    /// Every change in the working folder can be undone from a checkpoint
    /// (a code session, or a git repository). `auto` and `full` apply only
    /// while this holds: running unattended is justified by being able to take
    /// it back, not by the setting alone.
    reversible: AtomicBool,
    /// The project folder `auto` mode confines itself to. `None` = no folder
    /// known, so `auto` has nothing to judge against and every call asks.
    workspace: RwLock<Option<Arc<Scope>>>,
    /// Where a Bash command starts (the chat's working folder).
    workdir: RwLock<PathBuf>,
}

impl PermissionManager {
    pub(crate) fn new(event_bus: EventBus, response_registry: Arc<ResponseRegistry>) -> Self {
        Self {
            skip_file_edit: std::sync::atomic::AtomicBool::new(false),
            skip_bash: std::sync::atomic::AtomicBool::new(false),
            skip_skill: std::sync::atomic::AtomicBool::new(false),
            skip_mcp: std::sync::atomic::AtomicBool::new(false),
            allowed_tools: Mutex::new(HashSet::new()),
            global_edit_granted: Mutex::new(false),
            response_registry,
            event_bus,
            autonomy: AtomicU8::new(0),
            code_session: AtomicBool::new(false),
            reversible: AtomicBool::new(false),
            workspace: RwLock::new(None),
            workdir: RwLock::new(PathBuf::new()),
        }
    }

    pub fn set_autonomy(&self, level: Autonomy, code_session: bool) {
        let byte = match level {
            Autonomy::Ask => 0,
            Autonomy::Auto => 1,
            Autonomy::Full => 2,
        };
        self.autonomy.store(byte, AtomicOrdering::Relaxed);
        self.code_session.store(code_session, AtomicOrdering::Relaxed);
        let dir = self.workdir.read().unwrap().to_string_lossy().into_owned();
        self.reversible.store(
            crate::checkpoints::checkpointable_dir(&dir, code_session).is_some(),
            AtomicOrdering::Relaxed,
        );
    }

    /// The level in force: what was asked for, but never above `Ask` in a
    /// folder whose changes cannot be undone.
    pub fn autonomy(&self) -> Autonomy {
        if !self.reversible.load(AtomicOrdering::Relaxed) {
            return Autonomy::Ask;
        }
        match self.autonomy.load(AtomicOrdering::Relaxed) {
            1 => Autonomy::Auto,
            2 => Autonomy::Full,
            _ => Autonomy::Ask,
        }
    }

    /// Tell `auto` mode which folder is the project. Called with the engine's
    /// working directory at construction and whenever it changes.
    pub fn set_workspace(&self, dir: &str) {
        let dir = dir.trim();
        if dir.is_empty() {
            *self.workspace.write().unwrap() = None;
            *self.workdir.write().unwrap() = PathBuf::new();
            self.reversible.store(false, AtomicOrdering::Relaxed);
            return;
        }
        let path = crate::util::paths::expand_tilde(dir);
        *self.workspace.write().unwrap() = Some(Arc::new(Scope::new(&[path.clone()])));
        *self.workdir.write().unwrap() = path;
        self.reversible.store(
            crate::checkpoints::checkpointable_dir(dir, self.code_session.load(AtomicOrdering::Relaxed)).is_some(),
            AtomicOrdering::Relaxed,
        );
    }

    /// What `auto` mode makes of this call: `None` when the session is not in
    /// `auto` or the tool is not one it grades (so the ordinary rules decide),
    /// else the graded verdict.
    fn auto_assess(&self, name: &str, input: &serde_json::Value) -> Option<command_risk::Assessment> {
        if self.autonomy() != Autonomy::Auto {
            return None;
        }
        let scope = self.workspace.read().unwrap().clone()?;
        if FILE_EDIT_TOOLS.contains(&name) {
            let path = input
                .get("file_path")
                .or_else(|| input.get("notebook_path"))
                .and_then(|v| v.as_str())?;
            return Some(command_risk::assess_edit_path(path, &scope));
        }
        if name == "Bash" {
            let command = input.get("command").and_then(|v| v.as_str())?;
            let cwd = self.workdir.read().unwrap().clone();
            return Some(command_risk::assess_bash(command, &scope, &cwd));
        }
        None
    }

    /// `Some(reason)` when `auto` mode still wants a person for this call;
    /// `None` when it either let the call through (`ran`) or is not in play.
    /// Counts both outcomes.
    fn auto_decision(&self, name: &str, input: &serde_json::Value) -> AutoDecision {
        match self.auto_assess(name, input) {
            None => AutoDecision::NotApplicable,
            Some(a) if a.risk != Risk::High => {
                AUTO_RAN.fetch_add(1, AtomicOrdering::Relaxed);
                debug!("[PermissionManager] auto-run {name}: {} ({})", a.reason, a.risk.as_str());
                AutoDecision::Run
            }
            Some(a) => {
                AUTO_ASKED.fetch_add(1, AtomicOrdering::Relaxed);
                info!("[PermissionManager] auto asks about {name}: {}", a.reason);
                AutoDecision::Ask(a.reason)
            }
        }
    }

    pub fn update_skip_flags(&self, file_edit: bool, bash: bool, skill: bool, mcp: bool) {
        use std::sync::atomic::Ordering;
        self.skip_file_edit.store(file_edit, Ordering::Relaxed);
        self.skip_bash.store(bash, Ordering::Relaxed);
        self.skip_skill.store(skill, Ordering::Relaxed);
        self.skip_mcp.store(mcp, Ordering::Relaxed);
    }

    pub fn grant_global_edit(&self) {
        *self.global_edit_granted.lock().unwrap() = true;
        info!("Global edit permission granted for session");
    }

    pub fn add_allowed_tool(&self, key: &str) {
        self.allowed_tools.lock().unwrap().insert(key.to_owned());
    }

    // ============================================================
    // Internal helpers
    // ============================================================

    fn is_file_edit_tool(name: &str) -> bool {
        FILE_EDIT_TOOLS.contains(&name)
    }

    fn is_skill_tool(name: &str) -> bool {
        name == SKILL_TOOL_NAME
    }

    fn is_mcp_tool(name: &str) -> bool {
        name.starts_with(MCP_TOOL_PREFIX)
    }

    /// `mcp__<server>__<tool>` in any server spelling (`core`, `senclaw-browser`).
    fn is_per_call_mcp_tool(name: &str) -> bool {
        Self::is_mcp_tool(name)
            && name
                .rsplit("__")
                .next()
                .is_some_and(|tool| PER_CALL_MCP_TOOLS.contains(&tool))
    }

    /// The pending browser action a `browser_approve` call would release, in
    /// place of the bare `{approval_id, approve}` the agent sent.
    fn browser_approval_content(name: &str, input: &serde_json::Value) -> Option<serde_json::Value> {
        if !Self::is_per_call_mcp_tool(name) {
            return None;
        }
        let id = input.get("approval_id")?.as_str()?;
        let mut described = crate::browser_agent::run::describe_approval(id)?;
        described["approve"] = input.get("approve").cloned().unwrap_or(serde_json::Value::Null);
        Some(described)
    }

    fn is_allowed(&self, key: &str) -> bool {
        self.allowed_tools.lock().unwrap().contains(key)
    }

    fn get_permission_key(
        tool: &dyn Tool,
        input: &serde_json::Value,
        prefix: Option<&str>,
    ) -> String {
        // The executing tool's name, so a grant saved under an alias cannot
        // sit in a different bucket from the tool it actually runs.
        let name = tool.permission_name();
        if name == "Bash" {
            if let Some(p) = prefix {
                return format!("Bash({p}:*)");
            }
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            return format!("Bash({cmd})");
        }
        if Self::is_skill_tool(name) {
            let skill_name = input.get("skill").and_then(|v| v.as_str()).unwrap_or("");
            return if skill_name.is_empty() {
                name.to_string()
            } else {
                format!("{name}({skill_name})")
            };
        }
        name.to_string()
    }

    /// Provably read-only commands run without a prompt. Delegates to the
    /// shell-safety classifier: per-pipe-segment readonly whitelist, git
    /// readonly subcommands, dangerous-flag screens, redirection and
    /// injection detection (`cat a; rm -rf /` no longer rides on `cat`).
    fn is_safe_command(command: &str) -> bool {
        crate::util::shell_safety::is_readonly_safe_command(command)
    }

    /// Derive a deterministic prefix for the "never ask again" option.
    /// Multi-word commands get their first word (`git` gets `git <sub>`);
    /// commands with redirections or dangerous words get no prefix option —
    /// prefix matching only sees the first word, so `rm:*`/`echo … > f` must
    /// stay per-command.
    fn derive_prefix(command: &str) -> Option<String> {
        if crate::util::shell_safety::is_unsafe_for_prefix_auth(command) {
            return None;
        }
        let toks: Vec<&str> = command.split_whitespace().collect();
        if toks.len() < 2 {
            return None; // single word — the exact-command key covers it
        }
        if toks[0] == "git" {
            return Some(format!("git {}", toks[1]));
        }
        Some(toks[0].to_string())
    }

    /// Whether a saved `Bash(<prefix>:*)` entry covers this command. Saved
    /// prefixes never cover commands with redirections or dangerous words
    /// (defense in depth against over-broad stored grants).
    fn matches_saved_prefix(&self, command: &str) -> bool {
        let cmd = command.trim();
        if cmd.is_empty() || crate::util::shell_safety::is_unsafe_for_prefix_auth(cmd) {
            return false;
        }
        let allowed = self.allowed_tools.lock().unwrap();
        allowed.iter().any(|key| {
            key.strip_prefix("Bash(")
                .and_then(|rest| rest.strip_suffix(":*)"))
                .map(|p| !p.is_empty() && (cmd == p || cmd.starts_with(&format!("{p} "))))
                .unwrap_or(false)
        })
    }

    /// Labels for the three answers. The "allow" wording says "all chats"
    /// because that is now what it does: the bridge turns the answer into a
    /// global auto-accept rule alongside the per-chat grant. It previously read
    /// "in this project" while covering only the chat it was clicked in, so the
    /// same card came back in every new conversation.
    fn build_options(
        tool: &dyn Tool,
        input: &serde_json::Value,
        prefix: Option<&str>,
    ) -> HashMap<String, String> {
        let name = tool.permission_name();
        if name == "Bash" {
            let command = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(p) = prefix {
                let mut opts = HashMap::new();
                opts.insert("agree".into(), "Confirm".into());
                opts.insert(
                    "allow".into(),
                    format!("Confirm, never ask for `{p}` commands again (all chats)"),
                );
                opts.insert("refuse".into(), "Reject".into());
                return opts;
            }
            let allow_text = if command.is_empty() {
                "Confirm, never ask for this command again (all chats)".into()
            } else {
                format!("Confirm, never ask for `{command}` again (all chats)")
            };
            let mut opts = HashMap::new();
            opts.insert("agree".into(), "Confirm".into());
            opts.insert("allow".into(), allow_text);
            opts.insert("refuse".into(), "Reject".into());
            return opts;
        }

        if Self::is_file_edit_tool(name) {
            let mut opts = HashMap::new();
            opts.insert("agree".into(), "Confirm".into());
            opts.insert(
                "allow".into(),
                "Confirm, never ask for file editing again (all chats)".into(),
            );
            opts.insert("refuse".into(), "Reject".into());
            return opts;
        }

        if Self::is_skill_tool(name) {
            let skill_name = input.get("skill").and_then(|v| v.as_str()).unwrap_or("");
            let mut opts = HashMap::new();
            opts.insert("agree".into(), "Confirm".into());
            opts.insert(
                "allow".into(),
                format!("Confirm, never ask for {skill_name} Skill again (all chats)"),
            );
            opts.insert("refuse".into(), "Reject".into());
            return opts;
        }

        if Self::is_per_call_mcp_tool(name) {
            let mut opts = HashMap::new();
            opts.insert("agree".into(), "Confirm".into());
            opts.insert("refuse".into(), "Reject".into());
            return opts;
        }

        if Self::is_mcp_tool(name) {
            let mut opts = HashMap::new();
            opts.insert("agree".into(), "Confirm".into());
            opts.insert(
                "allow".into(),
                format!("Confirm, never ask for {name} again (all chats)"),
            );
            opts.insert("refuse".into(), "Reject".into());
            return opts;
        }

        let mut opts = HashMap::new();
        opts.insert("agree".into(), "Allow".into());
        opts.insert(
            "allow".into(),
            format!("Allow, never ask for {name} again (all chats)"),
        );
        opts.insert("refuse".into(), "Reject".into());
        opts
    }

    /// Resolve the `content` attached to a permission request.
    ///
    /// Uses a tool's custom `gen_tool_permission` content when present (e.g.
    /// `Bash` → `{"command": …}`), otherwise the raw tool `input`. The fallback
    /// matters for tools with no custom permission info — notably `Skill`: the
    /// auto-accept matcher reads the skill name from `content.get("skill")`
    /// (permission_bridge `SkillExact`), so a `Value::Null` here meant per-skill
    /// "Auto Accept" rules never matched and the prompt fired anyway (showing
    /// `null`). The input carries `{"skill": "<name>", …}`, restoring both the
    /// rule match and a meaningful prompt body.
    fn resolve_permission_content(
        permission_info: Option<ToolPermissionInfo>,
        input: &serde_json::Value,
    ) -> serde_json::Value {
        permission_info.map_or_else(|| input.clone(), |p| p.content)
    }

    /// Request permission via event and wait for response.
    async fn request_permission(
        &self,
        tool: &dyn Tool,
        input: &serde_json::Value,
        prefix: Option<&str>,
        cancel: &CancellationToken,
        agent_id: &str,
    ) -> Result<bool> {
        self.request_permission_noted(tool, input, prefix, None, cancel, agent_id).await
    }

    /// [`Self::request_permission`] with the reason `auto` mode is asking,
    /// appended to the card's title so the person sees why this one stopped.
    async fn request_permission_noted(
        &self,
        tool: &dyn Tool,
        input: &serde_json::Value,
        prefix: Option<&str>,
        note: Option<&str>,
        cancel: &CancellationToken,
        agent_id: &str,
    ) -> Result<bool> {
        let name = tool.name().to_string();
        let permission_info = tool.gen_tool_permission(input);
        let options = Self::build_options(tool, input, prefix);
        // Derived once and carried through the request so the UI persists the
        // exact key `is_allowed` will look up later.
        let permission_key = Self::get_permission_key(tool, input, prefix);

        let request = ToolPermissionRequestData {
            agent_id: agent_id.to_string(),
            tool_name: name.clone(),
            permission_key: permission_key.clone(),
            title: {
                let base = permission_info.as_ref().map_or(name.clone(), |p| p.title.clone());
                match note.filter(|n| !n.is_empty()) {
                    Some(n) => format!("{base} — {n}"),
                    None => base,
                }
            },
            content: Self::browser_approval_content(tool.permission_name(), input)
                .unwrap_or_else(|| Self::resolve_permission_content(permission_info, input)),
            options,
        };

        // Register before emitting the request so fast responders cannot race
        // ahead of the waiter.
        let mut rx = self.response_registry.register_tool_permission(&name);
        info!(
            "[PermissionManager] request emitted agent={} tool={} options={:?}",
            agent_id,
            name,
            request.options.keys().collect::<Vec<_>>()
        );

        // Emit to event bus (for UI)
        self.event_bus
            .emit(EngineEvent::ToolPermissionRequest(request));
        info!(
            "[PermissionManager] waiting response agent={} tool={}",
            agent_id, name
        );

        // Wait for response or cancellation
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(
                    "[PermissionManager] cancelled while waiting agent={} tool={}",
                    agent_id, name
                );
                Ok(false)
            }
            result = &mut rx => {
                match result {
                    Ok(response) => {
                        info!(
                            "[PermissionManager] response received agent={} tool={} selected={}",
                            agent_id, name, response.selected
                        );
                        match response.selected.as_str() {
                            "agree" => Ok(true),
                            "allow" => {
                                self.add_allowed_tool(&permission_key);
                                if Self::is_file_edit_tool(tool.permission_name()) {
                                    self.grant_global_edit();
                                }
                                Ok(true)
                            }
                            "refuse" => Ok(false),
                            _ => {
                                // Custom feedback — allow but with message
                                Ok(false)
                            }
                        }
                    }
                    Err(_) => {
                        // Sender dropped (engine disposed)
                        info!(
                            "[PermissionManager] response waiter dropped agent={} tool={}",
                            agent_id, name
                        );
                        Ok(false)
                    }
                }
            }
        }
    }
}

// ============================================================================
// PermissionChecker trait impl
// ============================================================================

#[async_trait]
impl PermissionChecker for PermissionManager {
    async fn check(
        &self,
        tool: &dyn Tool,
        input: &serde_json::Value,
        cancel: &CancellationToken,
        agent_id: &str,
    ) -> Result<bool> {
        use std::sync::atomic::Ordering;
        // The tool that will EXECUTE, not what the model called it. Every
        // branch below is a category test, and an alias is a display name: as
        // `tool.name()` an aliased `Edit` matched no branch at all and fell
        // through to "other non-readonly tool — default allow", writing files
        // with no prompt. `run_tools` already classifies read-only the same
        // way, through the resolver dispatch uses.
        let name = tool.permission_name();

        // `full` asks nobody — except for the calls that are the person's own
        // decision (see `PER_CALL_MCP_TOOLS`), which no mode may answer.
        if self.autonomy() == Autonomy::Full && !Self::is_per_call_mcp_tool(name) {
            return Ok(true);
        }

        // 1. File edit tools
        if Self::is_file_edit_tool(name) {
            if self.skip_file_edit.load(Ordering::Relaxed) {
                debug!("[{name}] skip file edit permission");
                return Ok(true);
            }
            if *self.global_edit_granted.lock().unwrap() {
                debug!("[{name}] global edit permission active");
                return Ok(true);
            }
            // A "never ask for file editing in this project" choice made in an
            // earlier session comes back as a saved `Edit`/`Write`/`NotebookEdit`
            // key. `global_edit_granted` is per-engine, so without this the
            // stored grant was dead weight and the prompt returned every time a
            // new engine was built. Any one of them re-grants the whole
            // category because that is what the option's label promised.
            if FILE_EDIT_TOOLS.iter().any(|t| self.is_allowed(t)) {
                debug!("[{name}] file edit permission restored from saved approval");
                self.grant_global_edit();
                return Ok(true);
            }
            let mut note = None;
            match self.auto_decision(name, input) {
                AutoDecision::Run => return Ok(true),
                AutoDecision::Ask(why) => note = Some(why),
                AutoDecision::NotApplicable => {}
            }
            return self
                .request_permission_noted(tool, input, None, note.as_deref(), cancel, agent_id)
                .await;
        }

        // 2. Bash tool
        if name == "Bash" {
            if self.skip_bash.load(Ordering::Relaxed) {
                return Ok(true);
            }
            let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let key = Self::get_permission_key(tool, input, None);
            if Self::is_safe_command(command)
                || self.is_allowed(&key)
                || self.matches_saved_prefix(command)
            {
                return Ok(true);
            }
            let mut note = None;
            match self.auto_decision(name, input) {
                AutoDecision::Run => return Ok(true),
                AutoDecision::Ask(why) => note = Some(why),
                AutoDecision::NotApplicable => {}
            }
            let prefix = Self::derive_prefix(command);
            return self
                .request_permission_noted(tool, input, prefix.as_deref(), note.as_deref(), cancel, agent_id)
                .await;
        }

        // 3. Skill tool
        if Self::is_skill_tool(name) {
            if self.skip_skill.load(Ordering::Relaxed) {
                return Ok(true);
            }
            let key = Self::get_permission_key(tool, input, None);
            if self.is_allowed(&key) {
                return Ok(true);
            }
            // A skill is instructions the person installed; loading one runs
            // nothing by itself, and every command it then suggests is graded.
            if self.autonomy() == Autonomy::Auto {
                return Ok(true);
            }
            return self
                .request_permission(tool, input, None, cancel, agent_id)
                .await;
        }

        // 4. MCP tools
        if Self::is_mcp_tool(name) {
            // Before any skip flag: skipping means nobody is asked, and nobody
            // asked is not the person's yes. Unattended runs (workflow steps,
            // background tasks) set every skip flag, so a paused purchase would
            // otherwise be released on the agent's word. It stays paused, for
            // the person to answer where SenClaw lists it.
            if Self::is_per_call_mcp_tool(name) {
                if self.skip_mcp.load(Ordering::Relaxed) {
                    anyhow::bail!(
                        "{name} needs the person's own yes and this session asks nobody; the action stays paused \
                         until they approve or decline it in SenClaw (Settings → Browser)"
                    );
                }
                return self.request_permission(tool, input, None, cancel, agent_id).await;
            }
            if self.skip_mcp.load(Ordering::Relaxed) {
                return Ok(true);
            }
            if self.is_allowed(name) {
                return Ok(true);
            }
            return self
                .request_permission(tool, input, None, cancel, agent_id)
                .await;
        }

        // Other non-readonly tools — default allow
        debug!("[{name}] non-standard tool, default allow");
        Ok(true)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    struct TestBashTool;
    #[async_trait::async_trait]
    impl Tool for TestBashTool {
        fn name(&self) -> &str {
            "Bash"
        }
        fn description(&self) -> &str {
            "Execute bash"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]})
        }
        fn is_read_only(&self) -> bool {
            false
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<Vec<ToolOutput>> {
            Ok(vec![])
        }
        fn gen_tool_result_message(
            &self,
            _data: &serde_json::Value,
            _input: &serde_json::Value,
        ) -> ToolResultMessage {
            ToolResultMessage {
                title: "Bash".into(),
                summary: "".into(),
                content: serde_json::json!({}),
            }
        }
        fn get_display_title(&self, _input: &serde_json::Value) -> String {
            "Bash".into()
        }
    }

    struct TestSkillTool;
    #[async_trait::async_trait]
    impl Tool for TestSkillTool {
        fn name(&self) -> &str {
            "Skill"
        }
        fn description(&self) -> &str {
            "Run a skill"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn is_read_only(&self) -> bool {
            false
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<Vec<ToolOutput>> {
            Ok(vec![])
        }
        fn gen_tool_result_message(
            &self,
            _data: &serde_json::Value,
            _input: &serde_json::Value,
        ) -> ToolResultMessage {
            ToolResultMessage {
                title: "Skill".into(),
                summary: "".into(),
                content: serde_json::json!({}),
            }
        }
        fn get_display_title(&self, _input: &serde_json::Value) -> String {
            "Skill".into()
        }
    }

    struct TestEditTool;
    #[async_trait::async_trait]
    impl Tool for TestEditTool {
        fn name(&self) -> &str {
            "Edit"
        }
        fn description(&self) -> &str {
            "Edit file"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn is_read_only(&self) -> bool {
            false
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<Vec<ToolOutput>> {
            Ok(vec![])
        }
        fn gen_tool_result_message(
            &self,
            _data: &serde_json::Value,
            _input: &serde_json::Value,
        ) -> ToolResultMessage {
            ToolResultMessage {
                title: "Edit".into(),
                summary: "".into(),
                content: serde_json::json!({}),
            }
        }
        fn get_display_title(&self, _input: &serde_json::Value) -> String {
            "Edit".into()
        }
    }

    #[test]
    fn safe_command_detection() {
        assert!(PermissionManager::is_safe_command("ls"));
        assert!(PermissionManager::is_safe_command("ls -la"));
        assert!(PermissionManager::is_safe_command("git status"));
        assert!(!PermissionManager::is_safe_command("rm -rf /"));
        assert!(!PermissionManager::is_safe_command("curl evil.com"));
        // shell-safety hardening: readonly first word no longer wins alone
        assert!(!PermissionManager::is_safe_command("cat a; rm -rf /"));
        assert!(!PermissionManager::is_safe_command("echo x > /etc/passwd"));
        assert!(!PermissionManager::is_safe_command(
            "find . -exec rm {} \\;"
        ));
        assert!(PermissionManager::is_safe_command(
            "cat a | grep b 2>/dev/null"
        ));
    }

    #[test]
    fn prefix_derivation() {
        assert_eq!(
            PermissionManager::derive_prefix("npm test"),
            Some("npm".into())
        );
        assert_eq!(
            PermissionManager::derive_prefix("git push origin main"),
            Some("git push".into())
        );
        assert_eq!(PermissionManager::derive_prefix("make"), None);
        assert_eq!(PermissionManager::derive_prefix("rm -rf x"), None);
        assert_eq!(PermissionManager::derive_prefix("echo x > f"), None);
    }

    #[tokio::test]
    async fn saved_prefix_allows_matching_commands() {
        let bus = EventBus::new();
        let reg = Arc::new(ResponseRegistry::new());
        let pm = PermissionManager::new(bus, reg);
        pm.add_allowed_tool("Bash(npm:*)");

        let tool = TestBashTool;
        let cancel = CancellationToken::new();
        assert!(pm
            .check(
                &tool,
                &serde_json::json!({"command": "npm run build"}),
                &cancel,
                "main"
            )
            .await
            .unwrap());
        // Prefix must not cover redirections (defense in depth)
        assert!(!pm.matches_saved_prefix("npm run build > /etc/passwd"));
        // Nor unrelated commands
        assert!(!pm.matches_saved_prefix("npx evil"));
    }

    struct NamedMcpTool(&'static str);
    #[async_trait::async_trait]
    impl Tool for NamedMcpTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "mcp tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn is_read_only(&self) -> bool {
            false
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<Vec<ToolOutput>> {
            Ok(vec![])
        }
        fn gen_tool_result_message(
            &self,
            _data: &serde_json::Value,
            _input: &serde_json::Value,
        ) -> ToolResultMessage {
            ToolResultMessage {
                title: self.0.into(),
                summary: "".into(),
                content: serde_json::json!({}),
            }
        }
        fn get_display_title(&self, _input: &serde_json::Value) -> String {
            self.0.into()
        }
    }

    /// A browser approval is the person's answer to one paused action: a
    /// saved "never ask again" must not answer it for them, and none is
    /// offered. Any other MCP tool keeps its saved grant. A cancelled token
    /// turns "asked" into `false`, so the two cases are told apart.
    #[tokio::test]
    async fn browser_approve_is_confirmed_on_every_call() {
        let pm = PermissionManager::new(EventBus::new(), Arc::new(ResponseRegistry::new()));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let input = serde_json::json!({"approval_id": "apv_1", "approve": true});
        for name in ["mcp__core__browser_approve", "mcp__senclaw-browser__browser_approve"] {
            pm.add_allowed_tool(name);
            let tool = NamedMcpTool(name);
            assert!(!pm.check(&tool, &input, &cancel, "main").await.unwrap(), "{name} was allowed by a saved grant");
            let options = PermissionManager::build_options(&tool, &input, None);
            assert!(!options.contains_key("allow"), "{name} offered a standing grant");
        }
        pm.add_allowed_tool("mcp__core__browser_look");
        let look = NamedMcpTool("mcp__core__browser_look");
        assert!(pm.check(&look, &serde_json::json!({}), &cancel, "main").await.unwrap());
    }

    /// Unattended sessions skip every prompt. For `browser_approve` that would
    /// mean the agent approving its own paused purchase, so it is refused.
    #[tokio::test]
    async fn skipping_prompts_never_releases_a_browser_approval() {
        let pm = PermissionManager::new(EventBus::new(), Arc::new(ResponseRegistry::new()));
        pm.update_skip_flags(true, true, true, true);
        let cancel = CancellationToken::new();
        let input = serde_json::json!({"approval_id": "apv_1", "approve": true});
        for name in ["mcp__core__browser_approve", "mcp__senclaw-browser__browser_approve"] {
            pm.add_allowed_tool(name);
            let refused = pm.check(&NamedMcpTool(name), &input, &cancel, "main").await;
            assert!(refused.is_err(), "{name} was released with nobody asked");
        }
        let look = NamedMcpTool("mcp__core__browser_look");
        assert!(pm.check(&look, &serde_json::json!({}), &cancel, "main").await.unwrap(), "other MCP tools still skip");
    }

    /// An alias is a display name. Renaming `Write` used to move it out of
    /// every category the checker knows — not file-edit, not Bash, not Skill,
    /// not `mcp__` — so it landed on "other non-readonly tool, default allow"
    /// and wrote files with no prompt at all.
    ///
    /// Discriminated by cancelling the token: the file-edit branch asks and a
    /// cancelled ask is a refusal, while the default-allow branch never asks
    /// and returns true. Before the fix this assertion is `true`.
    #[tokio::test]
    async fn an_aliased_file_tool_still_asks_for_permission() {
        use crate::tools::tool_alias::AliasedTool;

        let pm = PermissionManager::new(EventBus::new(), Arc::new(ResponseRegistry::new()));
        let aliased = AliasedTool::new(
            "save_file".into(),
            None,
            Arc::new(crate::tools::WriteTool) as Arc<dyn Tool>,
        );
        let cancel = CancellationToken::new();
        cancel.cancel();

        let allowed = pm
            .check(
                &aliased,
                &serde_json::json!({"file_path": "/tmp/x", "content": "x"}),
                &cancel,
                "main",
            )
            .await
            .unwrap();
        assert!(!allowed, "an aliased Write was allowed without being asked");
    }

    #[test]
    fn a_grant_is_keyed_by_the_tool_that_executes_not_the_alias() {
        use crate::tools::tool_alias::AliasedTool;

        let aliased = AliasedTool::new(
            "save_file".into(),
            None,
            Arc::new(crate::tools::WriteTool) as Arc<dyn Tool>,
        );
        assert_eq!(aliased.permission_name(), "Write");
        assert_eq!(
            PermissionManager::get_permission_key(&aliased, &serde_json::json!({}), None),
            "Write",
            "a grant saved under the alias would sit in a different bucket \
             from the tool it runs"
        );
    }

    #[test]
    fn file_edit_tool_detection() {
        assert!(PermissionManager::is_file_edit_tool("Edit"));
        assert!(PermissionManager::is_file_edit_tool("Write"));
        assert!(PermissionManager::is_file_edit_tool("NotebookEdit"));
        assert!(!PermissionManager::is_file_edit_tool("Read"));
    }

    #[test]
    fn permission_key_for_bash() {
        let tool = TestBashTool;
        let key = PermissionManager::get_permission_key(
            &tool,
            &serde_json::json!({"command": "npm test"}),
            None,
        );
        assert_eq!(key, "Bash(npm test)");

        let key_with_prefix = PermissionManager::get_permission_key(
            &tool,
            &serde_json::json!({"command": "npm test"}),
            Some("npm"),
        );
        assert_eq!(key_with_prefix, "Bash(npm:*)");
    }

    #[tokio::test]
    async fn skip_bash_bypasses_permission() {
        let bus = EventBus::new();
        let reg = Arc::new(ResponseRegistry::new());
        let pm = PermissionManager::new(bus, reg);
        pm.update_skip_flags(false, true, false, false);

        let tool = TestBashTool;
        let cancel = CancellationToken::new();
        assert!(pm
            .check(
                &tool,
                &serde_json::json!({"command": "rm -rf /"}),
                &cancel,
                "main"
            )
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn safe_command_bypasses_permission() {
        let bus = EventBus::new();
        let reg = Arc::new(ResponseRegistry::new());
        let pm = PermissionManager::new(bus, reg);

        let tool = TestBashTool;
        let cancel = CancellationToken::new();
        assert!(pm
            .check(
                &tool,
                &serde_json::json!({"command": "ls"}),
                &cancel,
                "main"
            )
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn allowed_tool_bypasses_permission() {
        let bus = EventBus::new();
        let reg = Arc::new(ResponseRegistry::new());
        let pm = PermissionManager::new(bus, reg);
        pm.add_allowed_tool("Bash(npm test)");

        let tool = TestBashTool;
        let cancel = CancellationToken::new();
        assert!(pm
            .check(
                &tool,
                &serde_json::json!({"command": "npm test"}),
                &cancel,
                "main"
            )
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn edit_tool_with_skip_flag_bypasses() {
        let bus = EventBus::new();
        let reg = Arc::new(ResponseRegistry::new());
        let pm = PermissionManager::new(bus, reg);
        pm.update_skip_flags(true, false, false, false);

        let tool = TestEditTool;
        let cancel = CancellationToken::new();
        assert!(pm
            .check(&tool, &serde_json::json!({}), &cancel, "main")
            .await
            .unwrap());
    }

    #[test]
    fn resolve_permission_content_falls_back_to_input_for_skill() {
        // Regression: the `Skill` tool has no `gen_tool_permission` → `None`.
        // The request content MUST then become the raw input so the auto-accept
        // matcher can read `content["skill"]`. Previously this was `Value::Null`,
        // so per-skill "Auto Accept" rules never matched and the prompt fired
        // anyway, displaying `null`.
        let input = serde_json::json!({ "skill": "ssh-guide", "args": "connect" });
        let content = PermissionManager::resolve_permission_content(None, &input);
        assert_eq!(
            content.get("skill").and_then(|v| v.as_str()),
            Some("ssh-guide"),
            "skill name must survive so SkillExact auto-accept rules match"
        );
        assert_eq!(content, input);

        // A tool WITH custom permission content keeps it (e.g. Bash's display body).
        let info = ToolPermissionInfo {
            title: "Run command".into(),
            content: serde_json::json!({ "command": "ls" }),
        };
        let content = PermissionManager::resolve_permission_content(Some(info), &input);
        assert_eq!(content, serde_json::json!({ "command": "ls" }));
    }

    #[tokio::test]
    async fn saved_skill_approval_bypasses_prompt() {
        // Regression: "never ask for <skill> Skill in this project" used to be
        // persisted as the bare tool name "Skill", while the gate looks the
        // grant up under `Skill(<name>)`. The two never matched, so every new
        // engine re-prompted for a skill the user had already approved — the
        // symptom being the same permission card returning again and again.
        let bus = EventBus::new();
        let reg = Arc::new(ResponseRegistry::new());
        let pm = PermissionManager::new(bus, reg);
        pm.add_allowed_tool("Skill(ai-office-run)");

        let tool = TestSkillTool;
        let cancel = CancellationToken::new();
        assert!(pm
            .check(
                &tool,
                &serde_json::json!({"skill": "ai-office-run"}),
                &cancel,
                "main"
            )
            .await
            .unwrap());

        // The grant stays scoped to the approved skill: a different one still
        // has to ask. `check` would block on a response, so assert on the key
        // instead of driving the request.
        assert_ne!(
            PermissionManager::get_permission_key(
                &tool,
                &serde_json::json!({"skill": "other-skill"}),
                None
            ),
            "Skill(ai-office-run)"
        );
    }

    #[tokio::test]
    async fn saved_file_edit_approval_bypasses_prompt() {
        // "never ask for file editing in this project" persists as the tool
        // name; `global_edit_granted` is per-engine, so without honouring the
        // saved key the prompt returned on every restart.
        let bus = EventBus::new();
        let reg = Arc::new(ResponseRegistry::new());
        let pm = PermissionManager::new(bus, reg);
        pm.add_allowed_tool("Write");

        let tool = TestEditTool; // a different file-edit tool than the one saved
        let cancel = CancellationToken::new();
        assert!(pm
            .check(&tool, &serde_json::json!({}), &cancel, "main")
            .await
            .unwrap());
    }

    #[test]
    fn permission_key_is_scoped_not_the_bare_tool_name() {
        let skill = TestSkillTool;
        assert_eq!(
            PermissionManager::get_permission_key(
                &skill,
                &serde_json::json!({"skill": "ai-office-run"}),
                None
            ),
            "Skill(ai-office-run)"
        );
        let bash = TestBashTool;
        assert_eq!(
            PermissionManager::get_permission_key(
                &bash,
                &serde_json::json!({"command": "git status"}),
                Some("git status")
            ),
            "Bash(git status:*)"
        );
    }

    // ------------------------------------------------------------------
    // Autonomy: `auto` / `full` run what a checkpoint can undo, ask about the rest
    // ------------------------------------------------------------------

    use std::time::Duration;

    /// A permission manager rooted in a fresh folder. `git` makes it a repository
    /// (checkpointed); `code` makes it a code session; neither is neither.
    fn pm_in(level: Autonomy, git: bool, code: bool) -> (PermissionManager, tempfile::TempDir, tokio::sync::broadcast::Receiver<EngineEvent>) {
        let dir = tempfile::tempdir().unwrap();
        if git {
            std::fs::create_dir(dir.path().join(".git")).unwrap();
        }
        let bus = EventBus::new();
        let rx = bus.subscribe();
        let pm = PermissionManager::new(bus, Arc::new(ResponseRegistry::new()));
        pm.set_autonomy(level, code);
        pm.set_workspace(&dir.path().to_string_lossy());
        (pm, dir, rx)
    }

    /// `Some(allowed)` when the call was decided without a person, `None` when it
    /// is waiting for one (the future never finishes on its own).
    async fn decided(pm: &PermissionManager, tool: &dyn Tool, input: serde_json::Value) -> Option<bool> {
        let cancel = CancellationToken::new();
        tokio::time::timeout(Duration::from_millis(250), pm.check(tool, &input, &cancel, "main"))
            .await
            .ok()
            .map(|r| r.unwrap())
    }

    fn bash(cmd: &str) -> serde_json::Value {
        serde_json::json!({ "command": cmd })
    }

    #[tokio::test]
    async fn auto_runs_the_dev_loop_and_asks_before_publishing() {
        let (pm, _dir, mut rx) = pm_in(Autonomy::Auto, true, false);
        let ran_before = AUTO_RAN.load(AtomicOrdering::Relaxed);
        for cmd in ["cargo test --lib foo", "npm run build", "git commit -m x", "mkdir -p src/new", "rm -rf target"] {
            assert_eq!(decided(&pm, &TestBashTool, bash(cmd)).await, Some(true), "{cmd} should run unattended");
        }
        assert!(AUTO_RAN.load(AtomicOrdering::Relaxed) >= ran_before + 5);
        assert!(rx.try_recv().is_err(), "nothing was asked");

        assert_eq!(decided(&pm, &TestBashTool, bash("git push origin main")).await, None, "a push waits for a person");
        match rx.try_recv() {
            Ok(EngineEvent::ToolPermissionRequest(r)) => {
                assert!(r.title.contains("publishes commits"), "the card says why: {}", r.title);
            }
            other => panic!("expected a permission request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn auto_edits_the_project_and_asks_about_anything_outside_it() {
        let (pm, dir, _rx) = pm_in(Autonomy::Auto, true, false);
        let inside = dir.path().join("src/main.rs");
        assert_eq!(decided(&pm, &TestEditTool, serde_json::json!({"file_path": inside})).await, Some(true));
        let env = dir.path().join(".env");
        assert_eq!(decided(&pm, &TestEditTool, serde_json::json!({"file_path": env})).await, None);
        assert_eq!(decided(&pm, &TestEditTool, serde_json::json!({"file_path": "/etc/hosts"})).await, None);
    }

    #[tokio::test]
    async fn auto_does_not_apply_where_nothing_can_be_undone() {
        // Not a repository, not a code session: no checkpoint, so every call asks as before.
        let (pm, _dir, _rx) = pm_in(Autonomy::Auto, false, false);
        assert_eq!(pm.autonomy(), Autonomy::Ask);
        assert_eq!(decided(&pm, &TestBashTool, bash("cargo test")).await, None);

        // A code session is checkpointed even without .git.
        let (pm, _dir, _rx) = pm_in(Autonomy::Auto, false, true);
        assert_eq!(pm.autonomy(), Autonomy::Auto);
        assert_eq!(decided(&pm, &TestBashTool, bash("cargo test")).await, Some(true));

        // No folder at all: nothing to confine to.
        let pm = PermissionManager::new(EventBus::new(), Arc::new(ResponseRegistry::new()));
        pm.set_autonomy(Autonomy::Auto, true);
        assert_eq!(decided(&pm, &TestBashTool, bash("cargo test")).await, None);
    }

    #[tokio::test]
    async fn ask_is_the_old_behaviour_and_a_saved_grant_still_wins() {
        let (pm, _dir, _rx) = pm_in(Autonomy::Ask, true, false);
        assert_eq!(decided(&pm, &TestBashTool, bash("cargo test")).await, None);
        assert_eq!(decided(&pm, &TestBashTool, bash("ls")).await, Some(true), "read-only never asked");

        let (pm, _dir, _rx) = pm_in(Autonomy::Auto, true, false);
        pm.add_allowed_tool("Bash(git push origin main)");
        assert_eq!(decided(&pm, &TestBashTool, bash("git push origin main")).await, Some(true), "the person already said yes to exactly this");
    }

    #[tokio::test]
    async fn full_asks_nothing_except_what_is_the_persons_own_decision() {
        let (pm, _dir, _rx) = pm_in(Autonomy::Full, true, false);
        assert_eq!(decided(&pm, &TestBashTool, bash("git push")).await, Some(true));
        assert_eq!(decided(&pm, &TestEditTool, serde_json::json!({"file_path": "/etc/hosts"})).await, Some(true));
        let approve = NamedMcpTool("mcp__core__browser_approve");
        assert_eq!(decided(&pm, &approve, serde_json::json!({"approval_id": "x"})).await, None, "no mode answers for the person");
    }

    #[tokio::test]
    async fn auto_loads_a_skill_without_asking() {
        let (pm, _dir, _rx) = pm_in(Autonomy::Auto, true, false);
        assert_eq!(decided(&pm, &TestSkillTool, serde_json::json!({"skill": "code"})).await, Some(true));
        let (pm, _dir, _rx) = pm_in(Autonomy::Ask, true, false);
        assert_eq!(decided(&pm, &TestSkillTool, serde_json::json!({"skill": "code"})).await, None);
    }

    #[test]
    fn autonomy_parses_the_words_a_person_would_type() {
        assert_eq!(Autonomy::parse("Auto"), Some(Autonomy::Auto));
        assert_eq!(Autonomy::parse(" ask "), Some(Autonomy::Ask));
        assert_eq!(Autonomy::parse("yolo"), Some(Autonomy::Full));
        assert_eq!(Autonomy::parse("sometimes"), None);
        assert_eq!(Autonomy::default(), Autonomy::Ask);
    }
}

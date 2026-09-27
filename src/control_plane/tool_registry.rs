//! Tool Registry metadata (§8 B7): `when_to_use`, `not_for`, `examples`,
//! `risk_tier`, `reversible`, `idempotent`, `concurrency_safe`,
//! `requires_preview`, `cancellable` for every built-in tool.
//!
//! Deliberately a side table keyed by tool name, not new methods on
//! [`crate::zen_core::Tool`]: the trait has ~30 implementors across the
//! codebase, and turning 9 new fields into required trait methods would
//! touch every one of them for a metadata concern that is purely descriptive
//! — nothing here changes what a tool does or how permission is decided
//! (that stays `Tool::is_read_only`/`permission_name`/the gate). A linter
//! test below enforces coverage instead of the compiler.
//!
//! `risk_tier`/`reversible` here are the tool's **static, worst-case**
//! classification — used for policy_gate's floor and for listing tools to a
//! spec's `choice` options (§5 "Tool: cờ rủi ro"). They are coarser than the
//! *dynamic*, per-call judgment [`crate::decision::gate`] already makes for
//! one specific tool (`Bash`, scored per invocation by Jev/Laya): a `Bash`
//! call the gate approves as reversible is still, at the registry level, "the
//! generic sandbox tool, risk_tier 3" — §8's own worked example
//! (`db.delete_rows`) is exactly this kind of fixed, per-tool-name row.
//!
//! **Alias lookup**: a wrapper (`tools::tool_alias::AliasedTool`) forwards
//! `permission_name()` to the tool that actually executes — look metadata up
//! by that name, not `Tool::name()`, the same rule permission classification
//! already follows.

#[derive(Debug, Clone, Copy)]
pub struct ToolMetadata {
    pub when_to_use: &'static str,
    pub not_for: &'static str,
    pub examples: &'static [&'static str],
    /// 0 (no side effect) .. 4 (irreversible + broad blast radius).
    pub risk_tier: u8,
    pub reversible: bool,
    pub idempotent: bool,
    pub concurrency_safe: bool,
    pub requires_preview: bool,
    pub cancellable: bool,
}

macro_rules! tool_row {
    ($name:literal, $when:literal, $not_for:literal, [$($ex:literal),+ $(,)?],
     risk=$risk:literal, reversible=$rev:literal, idempotent=$idem:literal,
     concurrency_safe=$conc:literal, requires_preview=$prev:literal, cancellable=$cancel:literal) => {
        ($name, ToolMetadata {
            when_to_use: $when,
            not_for: $not_for,
            examples: &[$($ex),+],
            risk_tier: $risk,
            reversible: $rev,
            idempotent: $idem,
            concurrency_safe: $conc,
            requires_preview: $prev,
            cancellable: $cancel,
        })
    };
}

/// One row per built-in tool name. `risk_tier`/`reversible` classify the
/// generic call — see module docs for why `Bash` is fixed at its worst case
/// here even though the gate scores individual commands more finely.
static TOOLS: &[(&str, ToolMetadata)] = &[
    tool_row!("Read", "Read a file's contents before editing or answering about it.", "Binary files, or when you only need to check existence.",
        ["Read {file_path: \"src/main.rs\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("Grep", "Search file contents by pattern across a project.", "Listing files without a content match — use Glob.",
        ["Grep {pattern: \"fn main\", path: \"src\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("Glob", "Find files by name pattern.", "Searching file contents — use Grep.",
        ["Glob {pattern: \"**/*.rs\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("Time", "Get the current date/time or convert between timezones.", "Anything not time-related.",
        ["Time {}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("ToolSearch", "Load a deferred tool's full schema before calling it.", "Tools already in the roster — check the tool list first.",
        ["ToolSearch {query: \"browser navigate\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("WebFetch", "Fetch and read a URL's content.", "Anything requiring authentication the agent does not hold, or a site known to block fetchers.",
        ["WebFetch {url: \"https://example.com\", prompt: \"summarize\"}"], risk=1, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("Write", "Create a new file or fully overwrite one, after the user confirmed the content.", "Small in-place edits — use Edit; the previous content is not diffed.",
        ["Write {file_path: \"notes.md\", content: \"...\"}"], risk=2, reversible=true, idempotent=true, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("Edit", "Make a targeted find-and-replace change to an existing file.", "Creating a new file — use Write.",
        ["Edit {file_path: \"a.py\", old_string: \"x=1\", new_string: \"x=2\"}"], risk=2, reversible=true, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("NotebookEdit", "Edit one cell of a Jupyter notebook.", "Plain text files — use Edit.",
        ["NotebookEdit {notebook_path: \"nb.ipynb\", cell_id: \"c1\", new_source: \"print(1)\"}"], risk=2, reversible=true, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("Bash", "Run a shell command — the general-purpose sandbox for exploration and anything with no dedicated tool.", "An action a dedicated, auditable tool already covers (prefer that tool for risk_tier >= 3 operations).",
        ["Bash {command: \"cargo test -p senclaw\"}"], risk=3, reversible=false, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("AskUser", "Ask the user a free-text question and wait for their reply.", "A question with a fixed set of good answers — use AskUserQuestion.",
        ["AskUser {question: \"Which branch should I target?\"}"], risk=0, reversible=true, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("AskUserQuestion", "Ask the user to pick from a short list of options.", "Open-ended questions — use AskUser.",
        ["AskUserQuestion {question: \"Proceed?\", options: [\"yes\", \"no\"]}"], risk=0, reversible=true, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("FormUI", "Render a structured form and wait for the user's submission.", "A single yes/no or short-list choice — use AskUserQuestion.",
        ["FormUI {title: \"Book a slot\", fields: [...]}"], risk=0, reversible=true, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("emit_widget", "Push a one-way rich widget (chart, image, clock) to the chat.", "Anything needing a user response — use FormUI/AskUserQuestion.",
        ["emit_widget {kind: \"chart\", data: {...}}"], risk=0, reversible=true, idempotent=false, concurrency_safe=true, requires_preview=false, cancellable=false),
    tool_row!("widget_list", "List widget kinds available to emit_widget.", "Rendering a widget itself — use emit_widget.",
        ["widget_list {}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("TodoWrite", "Record or update the session's todo list.", "Long-term memory — use the cognitive tools.",
        ["TodoWrite {todos: [{content: \"fix bug\", status: \"pending\"}]}"], risk=0, reversible=true, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("PersonaUpdate", "Edit the agent's own persona/behaviour file (SOUL.md-style) from an explicit user instruction.", "Editing arbitrary project files — use Edit/Write.",
        ["PersonaUpdate {instruction: \"be more concise\"}"], risk=1, reversible=true, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("ExitPlanMode", "Leave Plan mode and submit the plan for approval.", "Anything outside plan-mode workflows.",
        ["ExitPlanMode {plan: \"...\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=false, requires_preview=true, cancellable=true),
    tool_row!("EnterPlanMode", "Switch into Plan mode (read-only research before writing).", "Mid-execution — plan mode is for the start of a task.",
        ["EnterPlanMode {}"], risk=0, reversible=true, idempotent=true, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("Skill", "Load a specific skill's full instructions on demand.", "Tools already loaded — this is for skill bodies, not tool schemas.",
        ["Skill {name: \"web-research\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("Task", "Delegate a bounded subtask to a fresh sub-agent with its own context.", "Trivial steps the main agent can do in one tool call.",
        ["Task {description: \"research X\", prompt: \"...\"}"], risk=2, reversible=false, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("DispatchCreateParent", "Create a DAG of sub-tasks without running it yet.", "A single independent task — use Task.",
        ["DispatchCreateParent {tasks: [...]}"], risk=2, reversible=false, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("DispatchCreateParentAndRun", "Create and immediately run a DAG of sub-tasks, blocking until it finishes.", "A single independent task — use Task.",
        ["DispatchCreateParentAndRun {tasks: [...]}"], risk=3, reversible=false, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("DispatchTask", "Dispatch one more task into an existing DAG.", "Starting a brand-new DAG — use DispatchCreateParent.",
        ["DispatchTask {parent_id: \"p1\", task: {...}}"], risk=2, reversible=false, idempotent=false, concurrency_safe=false, requires_preview=false, cancellable=true),
    tool_row!("DispatchAllTasks", "Block until every task in a DAG reaches a terminal state.", "Checking status without blocking — use dispatch_status.",
        ["DispatchAllTasks {parent_id: \"p1\"}"], risk=1, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("DispatchListAgents", "List agents available for dispatch.", "Anything that changes state — this is read-only.",
        ["DispatchListAgents {}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("StopBgJob", "Stop a running background job.", "A job that already finished — check PeekBgJob first.",
        ["StopBgJob {job_id: \"j1\"}"], risk=1, reversible=false, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=false),
    tool_row!("PeekBgJob", "Check a background job's status/output without blocking.", "Starting a job — this only reads.",
        ["PeekBgJob {job_id: \"j1\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("CogAdd", "Add a fact to the cognitive memory graph (append-only).", "Correcting a wrong fact — add the correction, do not expect deletion.",
        ["CogAdd {fact: \"user prefers dark mode\"}"], risk=1, reversible=true, idempotent=false, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("CogSearch", "Search the cognitive memory graph.", "Full-text search over files — use Grep.",
        ["CogSearch {query: \"deployment process\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("CogRecall", "Recall memory relevant to the current context.", "Writing new memory — use CogAdd.",
        ["CogRecall {query: \"user's timezone\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("CogForget", "Mark a memory as no longer valid.", "Temporary session state — use TodoWrite.",
        ["CogForget {memory_id: \"m1\"}"], risk=1, reversible=false, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("CogStats", "Report cognitive memory graph statistics.", "Reading actual memory content — use CogRecall/CogSearch.",
        ["CogStats {}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("find_symbol", "Find where a symbol is defined via the repo map.", "A folder without a repo map — falls back gracefully, but Grep is more reliable there.",
        ["find_symbol {name: \"ZenEngine\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("find_references", "Find references to a symbol via the repo map.", "Renaming — this only finds, it does not edit.",
        ["find_references {name: \"ZenEngine\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("symbol_body", "Read one symbol's body via the repo map.", "Reading a whole file — use Read.",
        ["symbol_body {name: \"run_daemon\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("repo_map", "Get the project's repo map (tree-sitter + PageRank).", "A single file's content — use Read.",
        ["repo_map {}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("lsp_diagnostics", "Get language-server diagnostics for a file.", "Files with no LSP server on PATH for their language.",
        ["lsp_diagnostics {file_path: \"src/main.rs\"}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("lsp_status", "Check which language servers are active.", "Getting diagnostics themselves — use lsp_diagnostics.",
        ["lsp_status {}"], risk=0, reversible=true, idempotent=true, concurrency_safe=true, requires_preview=false, cancellable=true),
    tool_row!("task_done", "Signal that the conversation's task is finished (ReAct-style submit) — exits the tool-calling loop.", "An intermediate status update — only the true final answer.",
        ["task_done {}"], risk=0, reversible=true, idempotent=true, concurrency_safe=false, requires_preview=false, cancellable=false),
];

/// Every built-in tool name this registry must cover — `all_tools()`'s names
/// plus the ones the engine registers separately with session state
/// (`crate::tools::all_tools`'s own doc comment names them).
pub const ENGINE_REGISTERED_TOOL_NAMES: &[&str] =
    &["TodoWrite", "Skill", "Task", "ToolSearch", "EnterPlanMode", "DispatchCreateParent", "DispatchCreateParentAndRun", "DispatchTask", "DispatchAllTasks", "DispatchListAgents"];

pub fn metadata_for(name: &str) -> Option<&'static ToolMetadata> {
    TOOLS.iter().find(|(n, _)| *n == name).map(|(_, m)| m)
}

/// §8's fail-closed floor, checkable independent of any one engine: nothing
/// with `risk_tier >= 3` and not reversible may be treated as
/// auto-approvable. `policy_gate` enforces this at the actual gate seam; this
/// helper is what makes it checkable on the static table too.
pub fn never_auto_approvable(name: &str) -> bool {
    metadata_for(name).is_some_and(|m| m.risk_tier >= 3 && !m.reversible)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_tool_has_metadata() {
        let mut missing = Vec::new();
        for t in crate::tools::all_tools() {
            if metadata_for(t.name()).is_none() {
                missing.push(t.name().to_string());
            }
        }
        for name in ENGINE_REGISTERED_TOOL_NAMES {
            if metadata_for(name).is_none() {
                missing.push((*name).to_string());
            }
        }
        assert!(missing.is_empty(), "tools missing registry metadata: {missing:?}");
    }

    #[test]
    fn every_row_has_non_empty_descriptive_fields_and_a_sane_risk_tier() {
        for (name, m) in TOOLS {
            assert!(!m.when_to_use.is_empty(), "{name}: when_to_use");
            assert!(!m.not_for.is_empty(), "{name}: not_for");
            assert!(!m.examples.is_empty(), "{name}: examples");
            assert!(m.risk_tier <= 4, "{name}: risk_tier out of range");
        }
    }

    #[test]
    fn bash_is_the_never_auto_approvable_floor_case() {
        assert!(never_auto_approvable("Bash"));
        assert!(!never_auto_approvable("Read"));
        assert!(!never_auto_approvable("unknown-tool-name"), "an unknown tool is not silently flagged either way");
    }

    #[test]
    fn alias_lookup_uses_permission_name_not_the_wrapper_name() {
        // A wrapper renamed "save_file" over Edit reports Edit as its
        // permission_name (see zen_core::Tool::permission_name docs); the
        // metadata lookup must follow that name, not the alias.
        assert!(metadata_for("save_file").is_none());
        assert!(metadata_for("Edit").is_some());
    }
}

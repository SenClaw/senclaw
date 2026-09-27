//! Concrete tool implementations that implement [`zen_core::Tool`].
//!
//! Each tool mirrors the TS sema-core tool surface.

pub mod ask_user;
pub mod ask_user_question;
pub mod bash;
pub mod bg_jobs;
pub mod cognitive;
pub mod dispatch;
pub mod edit;
pub mod emit_widget;
pub mod enter_plan_mode;
pub mod edit_apply;
mod lsp_tools;
mod repo_map_tools;
pub mod exit_plan_mode;
pub mod form_ui;
pub mod glob;
pub mod grep;
pub mod launch_ui;
pub mod notebook_edit;
pub mod peek_bg_job;
pub mod persona_update;
pub mod read;
pub mod skill;
pub mod stop_bg_job;
pub mod task;
pub mod task_done;
pub mod time;
pub mod todo_write;
pub mod tool_alias;
pub mod tool_search;
pub mod web_fetch;
pub mod widget_list;
pub mod write;

use std::sync::Arc;

use crate::zen_core::Tool;

pub use ask_user::AskUserTool;
pub use ask_user_question::AskUserQuestionTool;
pub use bash::BashTool;
pub use cognitive::{CogAddTool, CogForgetTool, CogRecallTool, CogSearchTool, CogStatsTool};
pub use dispatch::{
    DispatchAllTasksTool, DispatchCreateParentAndRunTool, DispatchCreateParentTool,
    DispatchListAgentsTool, DispatchTaskTool, DispatchToolsConfig,
};
pub use edit::EditTool;
pub use emit_widget::EmitWidgetTool;
pub use enter_plan_mode::{EnterPlanFn, EnterPlanModeTool};
pub use lsp_tools::{LspDiagnosticsTool, LspStatusTool};
pub use repo_map_tools::{FindReferencesTool, FindSymbolTool, RepoMapTool, SymbolBodyTool};
pub use exit_plan_mode::ExitPlanModeTool;
pub use form_ui::FormUITool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use launch_ui::LaunchUITool;
pub use notebook_edit::NotebookEditTool;
pub use peek_bg_job::PeekBgJobTool;
pub use persona_update::PersonaUpdateTool;
pub use read::ReadTool;
pub use skill::SkillTool;
pub use stop_bg_job::StopBgJobTool;
pub use task::{AgentConfig, TaskTool};
pub use task_done::{TaskDoneTool, TASK_DONE_TOOL_NAME};
pub use time::TimeTool;
pub use todo_write::TodoWriteTool;
pub use tool_search::{DeferredToolsFn, ToolSearchTool};
pub use web_fetch::WebFetchTool;
pub use widget_list::WidgetListTool;
pub use write::WriteTool;

/// All built-in tools (without engine dependencies).
/// Tools that need engine state (TodoWrite, Skill, Task) must be
/// registered separately by the engine.
pub fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(AskUserTool),
        Arc::new(AskUserQuestionTool),
        // Declarative interactive form — rich sibling of AskUserQuestion.
        Arc::new(FormUITool),
        // One-way rich widget push (chart/image/clock/weather/video/audio +
        // Space-App widgets) — display-only sibling of FormUI (no user
        // response round-trip). `widget_list` is its deferred discovery half.
        Arc::new(EmitWidgetTool),
        Arc::new(WidgetListTool),
        Arc::new(BashTool),
        Arc::new(GlobTool),
        Arc::new(GrepTool),
        Arc::new(ReadTool),
        Arc::new(WriteTool),
        Arc::new(EditTool),
        Arc::new(NotebookEditTool),
        Arc::new(TimeTool),
        Arc::new(WebFetchTool),
        Arc::new(ExitPlanModeTool),
        Arc::new(PeekBgJobTool),
        Arc::new(StopBgJobTool),
        // ReAct-style "submit" signal — when called, conversation loop exits.
        // Lets the engine distinguish intermediate status from final answer.
        Arc::new(TaskDoneTool),
        // Cognitive memory — direct in-process access to the knowledge graph.
        // Was previously exposed via the senclaw-cognitive MCP server (P6).
        // Now built-in so every agent gets them by default with zero IPC
        // overhead. The MCP file is kept around for standalone stdio use.
        Arc::new(CogAddTool),
        Arc::new(CogSearchTool),
        Arc::new(CogRecallTool),
        Arc::new(CogForgetTool),
        Arc::new(CogStatsTool),
        // Persona-shaping: structured SOUL.md editor. Triggered by user
        // instructions like "from now on respond more concisely".
        Arc::new(PersonaUpdateTool),
        // Repo-map symbol tools (tree-sitter index shared with the
        // `<repo_map>` prompt block). Read-only; answer "not a project" in a
        // folder without a git repo or manifest.
        Arc::new(FindSymbolTool),
        Arc::new(FindReferencesTool),
        Arc::new(SymbolBodyTool),
        Arc::new(RepoMapTool),
        // Language-server diagnostics on demand (the automatic path is the
        // hook in run_tools after Edit/Write).
        Arc::new(LspDiagnosticsTool),
        Arc::new(LspStatusTool),
    ]
}

#[cfg(test)]
mod path_field_tests {
    use std::sync::Arc;

    use super::tool_alias::AliasedTool;
    use super::*;
    use crate::zen_core::Tool;

    /// Every tool that touches the filesystem must declare its path inputs.
    /// Miss one and `run_tools` skips resolution for it: the tool's own second
    /// layer still writes the right file, so nothing fails — but the approval
    /// card shows the user a path that is not the one being written, and that
    /// is exactly the failure the seam exists to prevent. Nothing else in the
    /// suite would notice.
    #[test]
    fn the_filesystem_tools_declare_their_path_inputs() {
        assert_eq!(ReadTool.path_fields(), ["file_path"]);
        assert_eq!(EditTool.path_fields(), ["file_path"]);
        assert_eq!(WriteTool.path_fields(), ["file_path"]);
        assert_eq!(NotebookEditTool.path_fields(), ["notebook_path"]);
        assert_eq!(GlobTool.path_fields(), ["path"]);
        assert_eq!(GrepTool.path_fields(), ["path"]);
    }

    #[test]
    fn a_tool_with_no_filesystem_path_declares_none() {
        // The reason declarations are per tool: an MCP tool's `path` can be a
        // wiki page or a URL path, and rewriting it would corrupt the call.
        assert!(BashTool.path_fields().is_empty());
    }

    #[test]
    fn an_alias_keeps_the_inner_tools_path_inputs() {
        let inner: Arc<dyn Tool> = Arc::new(WriteTool);
        let aliased = AliasedTool::new("save_file".into(), None, inner);
        assert_eq!(
            aliased.path_fields(),
            ["file_path"],
            "an aliased file tool bypasses the resolution seam"
        );
    }
}

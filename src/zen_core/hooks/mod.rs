//! Hook system for zen-core.
//!
//! Mirrors TS `sema-core/hooks/` — configurable shell command and LLM prompt
//! hooks that fire at key points in the agent lifecycle.
//!
//! ## Module layout
//!
//! ```text
//! hooks/
//!   types.rs           — HookEvent, HookDefinition, HookInput variants, HookOutput
//!   manager.rs         — HookManager (match events → definitions)
//!   command_executor.rs — execute shell command hooks
//!   prompt_executor.rs  — execute LLM prompt hooks
//!   executor.rs        — execute_hooks() top-level aggregator
//! ```
//!
//! ## Usage from ZenEngine
//!
//! ```rust,ignore
//! // On engine creation
//! let hook_manager = Arc::new(HookManager::empty());
//!
//! // To update config at runtime
//! hook_manager.update_config(loaded_config);
//!
//! // To fire hooks before tool execution (PreToolUse)
//! let result = execute_hooks(
//!     &hook_manager,
//!     &HookEvent::PreToolUse,
//!     &HookInput::PreToolUse(PreToolUseInput { ... }),
//!     &ExecuteHooksOptions { cancel: Some(&cancel), client: Some(&http_client), profile: Some(&profile), ..Default::default() },
//! ).await;
//! if result.blocked { /* deny tool execution */ }
//! ```

pub mod command_executor;
pub mod executor;
pub mod manager;
pub mod prompt_executor;
pub mod types;

pub use executor::{execute_hooks, ExecuteHooksOptions};
pub use manager::HookManager;
pub use types::{
    AggregatedHookResult, ErrorInput, HookConfig, HookDefinition, HookError, HookEvent,
    HookEventConfig, HookInput, HookInputBase, HookOutput, HookType, NotificationInput,
    OutputFilterInput, PermissionRequestInput, PostToolUseInput, PreCompactInput,
    PrePermissionInput, PreToolUseInput, SessionInput, StopInput, SubagentInput,
    UserPromptSubmitInput,
};

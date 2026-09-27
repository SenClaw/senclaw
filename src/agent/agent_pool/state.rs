//! Internal [`State`] struct — all mutable fields used by [`AgentPool`].

use std::collections::{HashMap, HashSet};

use super::traits::CachedTools;
use super::types::CachedTodos;
use super::types::{AbortFn, ActivityResetFn, CleanupFn, UnwatchFn};

/// Internal mutable state for [`AgentPool`]. All fields are accessed behind a
/// single [`std::sync::Mutex`].
pub(crate) struct State {
    /// JIDs with an active core (real type is hidden behind CoreApi).
    pub cores: HashSet<String>,
    /// jid → binding snapshot.
    pub bindings: HashMap<String, crate::types::GroupBinding>,

    // permission / thinking flags (runtime mirror of config.json).
    pub skip_main_agent_permissions: bool,
    pub skip_all_agents_permissions: bool,
    pub thinking_enabled: bool,

    // workspace tracking.
    pub runtime_work_dirs: HashMap<String, String>,
    pub workspace_watchers: HashMap<String, UnwatchFn>,

    // dispatch coordination.
    pub dispatch_workspace_overrides: HashMap<String, String>,
    pub dispatch_executing: HashSet<String>,
    pub last_dispatch_replies: HashMap<String, String>,
    pub dispatch_task_map: HashMap<String, String>,

    // process_and_wait event bridge (per-jid) — sender set by PAW before
    // process_user_input, forwarded to by bind_events persistent handlers.
    pub process_event_txs:
        HashMap<String, tokio::sync::mpsc::UnboundedSender<super::types::ProcessEvent>>,

    // process_and_wait runtime state.
    pub active_timer_resets: HashMap<String, ActivityResetFn>,
    pub active_aborts: HashMap<String, AbortFn>,
    pub event_cleanups: HashMap<String, CleanupFn>,

    // todos cache + create-lock + pause sets.
    pub cached_todos: HashMap<String, CachedTodos>,
    pub cached_tools: HashMap<String, CachedTools>,
    pub pending_creates: HashSet<String>,
    pub paused_children_by_admin: HashMap<String, Vec<String>>,
    pub synth_paused_jids: HashSet<String>,
    pub dispatch_paused_jids: HashSet<String>,

    /// Pending agent mode — stored when `set_agent_mode` is called before the
    /// engine exists (or after stop_and_clear). Applied in `ensure_agent` when
    /// the engine is (re)created.
    pub pending_agent_modes: HashMap<String, String>,
}

impl State {
    pub fn new() -> Self {
        Self {
            cores: HashSet::new(),
            bindings: HashMap::new(),
            skip_main_agent_permissions: false,
            skip_all_agents_permissions: false,
            thinking_enabled: true,
            runtime_work_dirs: HashMap::new(),
            workspace_watchers: HashMap::new(),
            dispatch_workspace_overrides: HashMap::new(),
            dispatch_executing: HashSet::new(),
            last_dispatch_replies: HashMap::new(),
            dispatch_task_map: HashMap::new(),
            process_event_txs: HashMap::new(),
            active_timer_resets: HashMap::new(),
            active_aborts: HashMap::new(),
            event_cleanups: HashMap::new(),
            cached_todos: HashMap::new(),
            cached_tools: HashMap::new(),
            pending_creates: HashSet::new(),
            paused_children_by_admin: HashMap::new(),
            synth_paused_jids: HashSet::new(),
            dispatch_paused_jids: HashSet::new(),
            pending_agent_modes: HashMap::new(),
        }
    }
}

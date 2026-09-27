//! Per-agent state management.
//!
//! Each [`StateManager`] holds message history, todos, file-read timestamps,
//! and a state machine for every agent. The main agent (`main`) is treated
//! specially — its state changes emit global events.
//!
//! Mirrors `StateManager` from TS sema-core.

use std::collections::HashMap;

use tracing::info;

use super::*;

/// Kind of a queued user input (mirrors TS `PendingUserInput['type']`).
///
/// `Command` (`/`-prefixed) inputs run solo as their own turn; `Inject`
/// inputs may be appended to an in-flight turn's tool results or batched
/// together into the next turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingInputKind {
    Command,
    Inject,
}

/// A user input that arrived while the main agent was processing.
#[derive(Debug, Clone)]
pub struct PendingUserInput {
    pub input: String,
    pub original_input: Option<String>,
    pub kind: PendingInputKind,
    /// Silent inputs (background/system-originated) skip user-facing events.
    pub silent: bool,
}

impl PendingUserInput {
    pub fn classify(input: &str, original_input: Option<&str>, silent: bool) -> Self {
        let kind = if input.trim_start().starts_with('/') {
            PendingInputKind::Command
        } else {
            PendingInputKind::Inject
        };
        Self {
            input: input.to_owned(),
            original_input: original_input.map(str::to_owned),
            kind,
            silent,
        }
    }
}

/// Take the next batch from a pending queue (mirrors TS `takeNextBatch`):
/// a leading command is taken alone; otherwise all inputs up to (excluding)
/// the next command are taken together.
pub fn take_next_batch(pending: &mut Vec<PendingUserInput>) -> Vec<PendingUserInput> {
    if pending.is_empty() {
        return Vec::new();
    }
    if pending[0].kind == PendingInputKind::Command {
        return pending.drain(..1).collect();
    }
    let next_command = pending
        .iter()
        .position(|p| p.kind == PendingInputKind::Command)
        .unwrap_or(pending.len());
    pending.drain(..next_command).collect()
}

/// Internal per-agent state record.
#[derive(Debug, Clone)]
struct AgentState {
    current_state: SessionState,
    previous_state: SessionState,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            current_state: SessionState::Idle,
            previous_state: SessionState::Idle,
        }
    }
}

pub struct StateManager {
    // === Per-agent isolated state ===
    states: HashMap<String, AgentState>,
    message_histories: HashMap<String, Vec<Message>>,
    read_file_timestamps: HashMap<String, HashMap<String, u64>>,
    todos: HashMap<String, Vec<TodosUpdateItem>>,

    // === Shared state ===
    session_id: Option<String>,
    global_edit_permission_granted: bool,
    plan_mode_info_sent: bool,
    pub(crate) current_abort: Option<tokio_util::sync::CancellationToken>,
    /// Inputs that arrived while the main agent was processing (FIFO).
    pending_inputs: Vec<PendingUserInput>,
}

impl Default for StateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl StateManager {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
            message_histories: HashMap::new(),
            read_file_timestamps: HashMap::new(),
            todos: HashMap::new(),
            session_id: None,
            global_edit_permission_granted: false,
            plan_mode_info_sent: false,
            current_abort: None,
            pending_inputs: Vec::new(),
        }
    }

    // ============================================================
    // Pending input queue (main agent)
    // ============================================================

    pub fn add_pending_input(&mut self, item: PendingUserInput) {
        self.pending_inputs.push(item);
    }

    pub fn pending_inputs_len(&self) -> usize {
        self.pending_inputs.len()
    }

    /// Take the next batch of queued inputs (see [`take_next_batch`]).
    pub fn take_next_input_batch(&mut self) -> Vec<PendingUserInput> {
        take_next_batch(&mut self.pending_inputs)
    }

    /// Drain leading `Inject` items, stopping at the first `Command`
    /// (mirrors TS `consumeInjectInputsBeforeNextCommand`). Used mid-turn to
    /// append fresh user messages to the current tool results.
    pub fn consume_inject_inputs_before_next_command(&mut self) -> Vec<PendingUserInput> {
        let cut = self
            .pending_inputs
            .iter()
            .position(|p| p.kind == PendingInputKind::Command)
            .unwrap_or(self.pending_inputs.len());
        self.pending_inputs.drain(..cut).collect()
    }

    // ============================================================
    // Session ID
    // ============================================================

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn set_session_id(&mut self, id: String) {
        info!("Session ID set: {id}, global edit permission reset");
        self.session_id = Some(id);
        self.global_edit_permission_granted = false;
    }

    // ============================================================
    // Message history (per agent)
    // ============================================================

    pub fn message_history(&self, agent_id: &str) -> Vec<Message> {
        self.message_histories
            .get(agent_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_message_history(&mut self, agent_id: &str, messages: Vec<Message>) {
        self.message_histories.insert(agent_id.to_owned(), messages);
    }

    // ============================================================
    // File read timestamps (per agent)
    // ============================================================

    pub fn read_timestamps(&self, agent_id: &str) -> HashMap<String, u64> {
        self.read_file_timestamps
            .get(agent_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_read_timestamp(&mut self, agent_id: &str, file_path: &str, ts: u64) {
        self.read_file_timestamps
            .entry(agent_id.to_owned())
            .or_default()
            .insert(file_path.to_owned(), ts);
    }

    // ============================================================
    // Todos (per agent)
    // ============================================================

    pub fn todos(&self, agent_id: &str) -> Vec<TodosUpdateItem> {
        self.todos.get(agent_id).cloned().unwrap_or_default()
    }

    pub fn set_todos(&mut self, agent_id: &str, items: Vec<TodosUpdateItem>) {
        self.todos.insert(agent_id.to_owned(), items);
    }

    /// Smart merge: if all new items already exist by `content`, update
    /// statuses in-place preserving order. Otherwise full replace.
    pub fn update_todos_intelligently(
        &mut self,
        agent_id: &str,
        new_todos: Vec<TodosUpdateItem>,
    ) -> Vec<TodosUpdateItem> {
        if new_todos.is_empty() {
            self.todos.insert(agent_id.to_owned(), new_todos.clone());
            return new_todos;
        }

        let current = self.todos(agent_id);
        let content_map: HashMap<&str, &TodosUpdateItem> =
            current.iter().map(|t| (t.content.as_str(), t)).collect();

        let is_status_only = new_todos
            .iter()
            .all(|t| content_map.contains_key(t.content.as_str()));

        if is_status_only && !current.is_empty() {
            let updated: Vec<TodosUpdateItem> = current
                .iter()
                .map(|existing| {
                    if let Some(update) = new_todos.iter().find(|t| t.content == existing.content) {
                        TodosUpdateItem {
                            content: existing.content.clone(),
                            status: update.status.clone(),
                            active_form: existing.active_form.clone(),
                        }
                    } else {
                        existing.clone()
                    }
                })
                .collect();
            self.todos.insert(agent_id.to_owned(), updated.clone());
            updated
        } else {
            self.todos.insert(agent_id.to_owned(), new_todos.clone());
            new_todos
        }
    }

    // ============================================================
    // Agent state machine (per agent)
    // ============================================================

    fn agent_state(&self, agent_id: &str) -> AgentState {
        self.states.get(agent_id).cloned().unwrap_or_default()
    }

    pub fn current_state(&self, agent_id: &str) -> SessionState {
        self.agent_state(agent_id).current_state
    }

    /// Update state for an agent. Returns the new state.
    pub fn update_state(&mut self, agent_id: &str, new_state: SessionState) -> SessionState {
        let entry = self.states.entry(agent_id.to_owned()).or_default();
        if entry.current_state != new_state {
            entry.previous_state = entry.current_state;
            entry.current_state = new_state;
            info!(
                "[{agent_id}] State: {:?} → {:?}",
                entry.previous_state, entry.current_state
            );
        }
        entry.current_state
    }

    // ============================================================
    // Finalize messages (checkpoint)
    // ============================================================

    /// Save messages and transition to idle (unless paused).
    pub fn finalize_messages(&mut self, agent_id: &str, messages: Vec<Message>) {
        self.set_message_history(agent_id, messages);
        if agent_id == MAIN_AGENT_ID && self.current_state(agent_id) == SessionState::Paused {
            return;
        }
        self.update_state(agent_id, SessionState::Idle);
    }

    // ============================================================
    // Global edit permission
    // ============================================================

    pub fn has_global_edit_permission(&self) -> bool {
        self.global_edit_permission_granted
    }

    pub fn grant_global_edit_permission(&mut self) {
        self.global_edit_permission_granted = true;
        info!("Global edit permission granted");
    }

    // ============================================================
    // Plan mode sent flag
    // ============================================================

    pub fn is_plan_mode_info_sent(&self) -> bool {
        self.plan_mode_info_sent
    }

    pub fn mark_plan_mode_info_sent(&mut self) {
        self.plan_mode_info_sent = true;
    }

    pub fn reset_plan_mode_info_sent(&mut self) {
        self.plan_mode_info_sent = false;
    }

    // ============================================================
    // Clear / reset
    // ============================================================

    pub fn clear_all(&mut self) {
        self.states.clear();
        self.message_histories.clear();
        self.read_file_timestamps.clear();
        self.todos.clear();
        self.current_abort = None;
        self.global_edit_permission_granted = false;
        self.plan_mode_info_sent = false;
        self.pending_inputs.clear();
        info!("All state cleared");
    }

    pub fn clear_agent(&mut self, agent_id: &str) {
        if agent_id != MAIN_AGENT_ID {
            self.states.remove(agent_id);
            self.message_histories.remove(agent_id);
            self.read_file_timestamps.remove(agent_id);
            self.todos.remove(agent_id);
            info!("[{agent_id}] State cleared");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_idle() {
        let sm = StateManager::new();
        assert_eq!(sm.current_state("main"), SessionState::Idle);
    }

    #[test]
    fn state_transitions() {
        let mut sm = StateManager::new();
        sm.update_state("main", SessionState::Processing);
        assert_eq!(sm.current_state("main"), SessionState::Processing);
        sm.update_state("main", SessionState::Idle);
        assert_eq!(sm.current_state("main"), SessionState::Idle);
    }

    #[test]
    fn finalize_does_not_clobber_paused() {
        let mut sm = StateManager::new();
        sm.update_state("main", SessionState::Processing);
        sm.update_state("main", SessionState::Paused);
        sm.finalize_messages("main", vec![]);
        assert_eq!(sm.current_state("main"), SessionState::Paused);
    }

    #[test]
    fn todos_smart_update_preserves_order() {
        let mut sm = StateManager::new();
        sm.set_todos(
            "main",
            vec![
                TodosUpdateItem {
                    content: "a".into(),
                    status: "pending".into(),
                    active_form: None,
                },
                TodosUpdateItem {
                    content: "b".into(),
                    status: "pending".into(),
                    active_form: None,
                },
            ],
        );

        let updated = sm.update_todos_intelligently(
            "main",
            vec![TodosUpdateItem {
                content: "a".into(),
                status: "completed".into(),
                active_form: None,
            }],
        );

        assert_eq!(updated.len(), 2);
        assert_eq!(updated[0].status, "completed");
        assert_eq!(updated[1].status, "pending");
    }

    #[test]
    fn todos_full_replace_when_new_items() {
        let mut sm = StateManager::new();
        sm.set_todos(
            "main",
            vec![TodosUpdateItem {
                content: "a".into(),
                status: "pending".into(),
                active_form: None,
            }],
        );

        let updated = sm.update_todos_intelligently(
            "main",
            vec![TodosUpdateItem {
                content: "c".into(),
                status: "pending".into(),
                active_form: None,
            }],
        );

        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].content, "c");
    }

    fn pi(input: &str) -> PendingUserInput {
        PendingUserInput::classify(input, None, false)
    }

    #[test]
    fn classify_commands_and_injects() {
        assert_eq!(pi("/compact").kind, PendingInputKind::Command);
        assert_eq!(pi("  /clear").kind, PendingInputKind::Command);
        assert_eq!(pi("hello").kind, PendingInputKind::Inject);
        assert_eq!(pi("what about a/b?").kind, PendingInputKind::Inject);
    }

    #[test]
    fn take_next_batch_command_runs_solo() {
        let mut q = vec![pi("/compact"), pi("hi"), pi("there")];
        let batch = take_next_batch(&mut q);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].input, "/compact");
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn take_next_batch_groups_until_command() {
        let mut q = vec![pi("a"), pi("b"), pi("/cmd"), pi("c")];
        let batch = take_next_batch(&mut q);
        assert_eq!(
            batch.iter().map(|i| i.input.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(q[0].input, "/cmd");
        // Next batch is the command alone.
        let batch = take_next_batch(&mut q);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].input, "/cmd");
        // Then the trailing normal input.
        let batch = take_next_batch(&mut q);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].input, "c");
        assert!(take_next_batch(&mut q).is_empty());
    }

    #[test]
    fn consume_inject_stops_at_command() {
        let mut sm = StateManager::new();
        sm.add_pending_input(pi("first"));
        sm.add_pending_input(pi("second"));
        sm.add_pending_input(pi("/cmd"));
        sm.add_pending_input(pi("third"));

        let injected = sm.consume_inject_inputs_before_next_command();
        assert_eq!(
            injected
                .iter()
                .map(|i| i.input.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        // Command and everything after it stay queued.
        assert_eq!(sm.pending_inputs_len(), 2);
        let batch = sm.take_next_input_batch();
        assert_eq!(batch[0].input, "/cmd");
    }

    #[test]
    fn clear_all_drops_pending_inputs() {
        let mut sm = StateManager::new();
        sm.add_pending_input(pi("queued"));
        sm.clear_all();
        assert_eq!(sm.pending_inputs_len(), 0);
    }

    #[test]
    fn per_agent_isolation() {
        let mut sm = StateManager::new();
        sm.set_message_history("main", vec![]);
        sm.set_message_history("sub-1", vec![]);
        sm.update_state("main", SessionState::Processing);
        sm.update_state("sub-1", SessionState::Processing);

        sm.clear_agent("sub-1");
        // Sub-agent cleared
        assert!(sm.message_history("sub-1").is_empty());
        // Main unaffected
        assert_eq!(sm.current_state("main"), SessionState::Processing);
    }
}

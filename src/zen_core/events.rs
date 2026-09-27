//! Event bus for zen-core engine events.
//!
//! Uses `tokio::sync::broadcast` so multiple consumers can receive every event.
//! The sender is clonable and cheap — each listener holds a receiver.

use std::collections::HashMap;

use tokio::sync::broadcast;

use super::workbench::{
    WorkbenchNewData, WorkbenchServiceCrashedData, WorkbenchServiceReadyData,
    WorkbenchServiceStoppedData,
};
use super::*;

/// Capacity for the broadcast channel. Must be large enough to absorb bursts
/// from streaming responses (thinking/text chunks) without lagging slow receivers.
const CHANNEL_CAPACITY: usize = 512;

/// All events the engine can emit. Mirror of the TS sema-core event surface.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    SessionReady(SessionReadyData),
    SessionInterrupted(SessionInterruptedData),
    SessionError(SessionErrorData),
    SessionCleared {
        session_id: Option<String>,
    },
    StateUpdate(StateUpdateData),
    InputReceived(InputReceivedData),
    MessageComplete(MessageCompleteData),
    ConversationUsage(ConversationUsageData),
    /// Per-LLM-call token accounting (bus-only; consumed by the embedding
    /// layer which adds the jid and feeds the usage recorder).
    LlmUsage(LlmUsageData),
    ThinkingChunk(ThinkingChunkData),
    TextChunk(TextChunkData),
    ToolPermissionRequest(ToolPermissionRequestData),
    ToolPermissionResponse(ToolPermissionResponseData),
    ToolExecutionComplete(ToolExecutionCompleteData),
    ToolExecutionError(ToolExecutionErrorData),
    TodosUpdate(Vec<TodosUpdateItem>),
    TopicUpdate(TopicUpdateData),
    CompactStart(CompactStartData),
    CompactExec(CompactExecData),
    FileReference(FileReferenceData),
    AskQuestionRequest(AskQuestionRequestData),
    AskQuestionResponse(AskQuestionResponseData),
    FormRequest(FormRequestData),
    FormResponse(FormResponseData),
    /// One-way rich widget push (display-only, no response). Mirrors the
    /// `tool:execution` push pattern rather than the `FormRequest` round-trip.
    WidgetEmit(WidgetEmitData),
    PlanExitRequest(PlanExitRequestData),
    PlanExitResponse(PlanExitResponseData),
    PlanImplement(PlanImplementData),
    TaskAgentStart(TaskAgentStartData),
    TaskAgentEnd(TaskAgentEndData),
    ConfigNoModels(ConfigNoModelsData),
    WorkbenchNew(WorkbenchNewData),
    WorkbenchServiceReady(WorkbenchServiceReadyData),
    WorkbenchServiceCrashed(WorkbenchServiceCrashedData),
    WorkbenchServiceStopped(WorkbenchServiceStoppedData),
}

/// Pub-sub event bus. Cheap to clone — wraps a `broadcast::Sender`.
///
/// ```ignore
/// let bus = EventBus::new();
/// let mut rx = bus.subscribe();
/// bus.emit(EngineEvent::StateUpdate(StateUpdateData { state: SessionState::Idle }));
/// ```
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<EngineEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Emit an event to all subscribers. Fire-and-forget — if no receivers
    /// exist the event is dropped silently.
    pub fn emit(&self, event: EngineEvent) {
        let _ = self.tx.send(event);
    }

    /// Get a new receiver. The receiver only sees events sent *after* this call.
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.tx.subscribe()
    }

    /// Number of active receivers.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

// ============================================================================
// One-shot request-response helpers
// ============================================================================

/// Per-instance registry for pairing permission/ask-question requests
/// with their response channels. Used when the engine suspends a tool
/// waiting for a user response.
pub struct ResponseRegistry {
    /// Pending tool permission responses, keyed by tool_name.
    tool_permission_txs:
        std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<ToolPermissionResponseData>>>,
    /// Pending ask-question responses, keyed by agent_id.
    ask_question_txs:
        std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<AskQuestionResponseData>>>,
    /// Pending FormUI responses, keyed by agent_id.
    form_txs: std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<FormResponseData>>>,
}

impl ResponseRegistry {
    pub fn new() -> Self {
        Self {
            tool_permission_txs: std::sync::Mutex::new(HashMap::new()),
            ask_question_txs: std::sync::Mutex::new(HashMap::new()),
            form_txs: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Register a waiter for a tool permission response.
    pub fn register_tool_permission(
        &self,
        tool_name: &str,
    ) -> tokio::sync::oneshot::Receiver<ToolPermissionResponseData> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tool_permission_txs
            .lock()
            .unwrap()
            .insert(tool_name.to_owned(), tx);
        rx
    }

    /// Deliver a tool permission response to the waiter.
    pub fn deliver_tool_permission(&self, response: ToolPermissionResponseData) -> bool {
        if let Some(tx) = self
            .tool_permission_txs
            .lock()
            .unwrap()
            .remove(&response.tool_name)
        {
            let _ = tx.send(response);
            true
        } else {
            false
        }
    }

    /// Register a waiter for an ask-question response.
    pub fn register_ask_question(
        &self,
        agent_id: &str,
    ) -> tokio::sync::oneshot::Receiver<AskQuestionResponseData> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.ask_question_txs
            .lock()
            .unwrap()
            .insert(agent_id.to_owned(), tx);
        rx
    }

    /// Deliver an ask-question response to the waiter.
    pub fn deliver_ask_question(&self, response: AskQuestionResponseData) -> bool {
        if let Some(tx) = self
            .ask_question_txs
            .lock()
            .unwrap()
            .remove(&response.agent_id)
        {
            let _ = tx.send(response);
            true
        } else {
            false
        }
    }

    /// Register a waiter for a FormUI response.
    pub fn register_form(
        &self,
        agent_id: &str,
    ) -> tokio::sync::oneshot::Receiver<FormResponseData> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.form_txs
            .lock()
            .unwrap()
            .insert(agent_id.to_owned(), tx);
        rx
    }

    /// Deliver a FormUI response to the waiter.
    pub fn deliver_form(&self, response: FormResponseData) -> bool {
        if let Some(tx) = self.form_txs.lock().unwrap().remove(&response.agent_id) {
            let _ = tx.send(response);
            true
        } else {
            false
        }
    }

    /// Remove all pending waiters.
    pub fn clear(&self) {
        self.tool_permission_txs.lock().unwrap().clear();
        self.ask_question_txs.lock().unwrap().clear();
        self.form_txs.lock().unwrap().clear();
    }
}

impl Default for ResponseRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_bus_emit_and_receive() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.emit(EngineEvent::StateUpdate(StateUpdateData {
            state: SessionState::Processing,
        }));

        // tokio::sync::broadcast receiver needs async context to recv,
        // but try_recv works synchronously for already-sent messages.
        let event = rx.try_recv().unwrap();
        match event {
            EngineEvent::StateUpdate(data) => assert_eq!(data.state, SessionState::Processing),
            _ => panic!("unexpected event"),
        }
    }

    #[test]
    fn event_bus_multiple_subscribers() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.emit(EngineEvent::StateUpdate(StateUpdateData {
            state: SessionState::Idle,
        }));

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }

    #[test]
    fn event_bus_no_receivers_is_noop() {
        let bus = EventBus::new();
        // No subscribers — should not panic
        bus.emit(EngineEvent::StateUpdate(StateUpdateData {
            state: SessionState::Idle,
        }));
    }

    #[test]
    fn response_registry_tool_permission() {
        let reg = ResponseRegistry::new();
        let mut rx = reg.register_tool_permission("Bash");
        let delivered = reg.deliver_tool_permission(ToolPermissionResponseData {
            tool_name: "Bash".into(),
            selected: "agree".into(),
        });
        assert!(delivered);
        let response = rx.try_recv().unwrap();
        assert_eq!(response.selected, "agree");
    }

    #[test]
    fn response_registry_unknown_tool() {
        let reg = ResponseRegistry::new();
        let delivered = reg.deliver_tool_permission(ToolPermissionResponseData {
            tool_name: "Nope".into(),
            selected: "agree".into(),
        });
        assert!(!delivered);
    }

    #[test]
    fn response_registry_form_round_trip() {
        let reg = ResponseRegistry::new();
        let mut rx = reg.register_form("main");
        let mut values = HashMap::new();
        values.insert("name".to_string(), serde_json::json!("Alice"));
        values.insert("count".to_string(), serde_json::json!(3));
        let delivered = reg.deliver_form(FormResponseData {
            agent_id: "main".into(),
            values,
            submitted: true,
        });
        assert!(delivered);
        let response = rx.try_recv().unwrap();
        assert!(response.submitted);
        assert_eq!(response.values["name"], serde_json::json!("Alice"));
    }

    #[test]
    fn response_registry_form_unknown_agent() {
        let reg = ResponseRegistry::new();
        let delivered = reg.deliver_form(FormResponseData {
            agent_id: "nobody".into(),
            values: HashMap::new(),
            submitted: false,
        });
        assert!(!delivered);
    }
}

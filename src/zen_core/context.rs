//! Engine Context - per-engine isolation using task local storage.
//!
//! Port of TS `core/EngineContext.ts` using tokio::task_local instead of AsyncLocalStorage.
//!
//! ## Architecture
//!
//! ```text
//! EngineStore (per-engine resources)
//!   ├── instanceId
//!   ├── workingDir
//!   ├── agentDataDir
//!   ├── coreConfig
//!   ├── eventBus
//!   ├── stateManager
//!   ├── mcpManager
//!   ├── hookManager
//!   └── ...
//!
//! run_with_engine(store, || async { ... })
//!   → Sets task_local ENGINE_STORE
//!   → All nested calls can access get_engine_store()
//!   → Automatic cleanup when scope ends
//! ```

use std::sync::{Arc, Mutex};

use super::{events::EventBus, hooks::HookManager, state::StateManager, ModelProfile};

/// Per-engine isolated resources.
#[derive(Clone)]
pub struct EngineStore {
    pub instance_id: String,
    pub working_dir: String,
    /// Agent 人设/配置目录（SOUL.md、.sema/）。未提供时等于 workingDir。
    pub agent_data_dir: String,
    pub core_config: CoreConfig,
    pub event_bus: EventBus,
    pub state_manager: Arc<Mutex<StateManager>>,
    pub mcp_manager: Option<Arc<crate::mcp::manager::McpManager>>,
    pub hook_manager: Arc<HookManager>,
}

/// Core configuration (subset of full config needed for context).
#[derive(Clone)]
pub struct CoreConfig {
    pub model_profile: ModelProfile,
    pub thinking: bool,
    pub stream: bool,
    pub agent_mode: String,
    pub use_tools: Vec<String>,
}

tokio::task_local! {
    static ENGINE_STORE: Option<EngineStore>;
}

/// Run a function within the given engine's context.
///
/// All calls to `get_engine_store()` inside `fn` (including through async boundaries)
/// will return engine-specific instances.
///
/// # Example
///
/// (`ignore` — illustrative pseudo-code; `no_run` would still compile it and
/// the `{ ... }` initializer is intentionally elided.)
///
/// ```rust,ignore
/// let store = EngineStore { ... };
/// run_with_engine(store, || async {
///     let current = get_engine_store().unwrap();
///     // Use current.event_bus, current.state_manager, etc.
/// }).await;
/// ```
pub fn run_with_engine<F, Fut, T>(store: EngineStore, f: F) -> impl std::future::Future<Output = T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T> + Send,
    T: Send + 'static,
{
    ENGINE_STORE.scope(Some(store), f())
}

/// Get the current engine's store if inside `run_with_engine()`, otherwise `None`.
pub fn get_engine_store() -> Option<EngineStore> {
    // `with` PANICS when called outside a `run_with_engine` scope (task-local
    // not set) — `try_with` is the "outside = None" contract this fn promises.
    ENGINE_STORE.try_with(|store| store.clone()).ok().flatten()
}

/// Helper: Get current event bus if in engine context.
pub fn get_event_bus() -> Option<EventBus> {
    get_engine_store().map(|s| s.event_bus.clone())
}

/// Helper: Get current state manager if in engine context.
pub fn get_state_manager() -> Option<Arc<Mutex<StateManager>>> {
    get_engine_store().map(|s| s.state_manager.clone())
}

/// Helper: Get current hook manager if in engine context.
pub fn get_hook_manager() -> Option<Arc<HookManager>> {
    get_engine_store().map(|s| s.hook_manager)
}

/// Helper: Get current MCP manager if in engine context.
pub fn get_mcp_manager() -> Option<Arc<crate::mcp::manager::McpManager>> {
    get_engine_store().and_then(|s| s.mcp_manager)
}

/// Helper: Get current working directory if in engine context.
pub fn get_working_dir() -> Option<String> {
    get_engine_store().map(|s| s.working_dir)
}

/// Helper: Get current agent data directory if in engine context.
pub fn get_agent_data_dir() -> Option<String> {
    get_engine_store().map(|s| s.agent_data_dir)
}

/// Helper: Get current model profile if in engine context.
pub fn get_model_profile() -> Option<ModelProfile> {
    get_engine_store().map(|s| s.core_config.model_profile.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::events::EventBus;

    #[tokio::test]
    async fn test_engine_context_isolation() {
        let store1 = EngineStore {
            instance_id: "engine1".to_string(),
            working_dir: "/dir1".to_string(),
            agent_data_dir: "/dir1/.sema".to_string(),
            core_config: CoreConfig {
                model_profile: ModelProfile {
                    name: "test1".to_string(),
                    provider: "test".to_string(),
                    model_name: "test1".to_string(),
                    base_url: "http://test".to_string(),
                    api_key: "test".to_string(),
                    max_tokens: 1000,
                    context_length: 4000,
                    adapt: None,
                    vision: None,
                    ..Default::default()
                },
                thinking: false,
                stream: false,
                agent_mode: "Agent".to_string(),
                use_tools: vec![],
            },
            event_bus: EventBus::new(),
            state_manager: Arc::new(Mutex::new(StateManager::new())),
            mcp_manager: None,
            hook_manager: Arc::new(HookManager::empty()),
        };

        let store2 = EngineStore {
            instance_id: "engine2".to_string(),
            working_dir: "/dir2".to_string(),
            agent_data_dir: "/dir2/.sema".to_string(),
            core_config: CoreConfig {
                model_profile: ModelProfile {
                    name: "test2".to_string(),
                    provider: "test".to_string(),
                    model_name: "test2".to_string(),
                    base_url: "http://test".to_string(),
                    api_key: "test".to_string(),
                    max_tokens: 1000,
                    context_length: 4000,
                    adapt: None,
                    vision: None,
                    ..Default::default()
                },
                thinking: false,
                stream: false,
                agent_mode: "Agent".to_string(),
                use_tools: vec![],
            },
            event_bus: EventBus::new(),
            state_manager: Arc::new(Mutex::new(StateManager::new())),
            mcp_manager: None,
            hook_manager: Arc::new(HookManager::empty()),
        };

        let result1 = run_with_engine(store1.clone(), || async {
            get_engine_store().map(|s| s.instance_id.clone())
        })
        .await;

        let result2 = run_with_engine(store2.clone(), || async {
            get_engine_store().map(|s| s.instance_id.clone())
        })
        .await;

        assert_eq!(result1, Some("engine1".to_string()));
        assert_eq!(result2, Some("engine2".to_string()));

        // Outside context, should be None
        assert!(get_engine_store().is_none());
    }

    #[tokio::test]
    async fn test_nested_context() {
        let store = EngineStore {
            instance_id: "outer".to_string(),
            working_dir: "/outer".to_string(),
            agent_data_dir: "/outer/.sema".to_string(),
            core_config: CoreConfig {
                model_profile: ModelProfile {
                    name: "test".to_string(),
                    provider: "test".to_string(),
                    model_name: "test".to_string(),
                    base_url: "http://test".to_string(),
                    api_key: "test".to_string(),
                    max_tokens: 1000,
                    context_length: 4000,
                    adapt: None,
                    vision: None,
                    ..Default::default()
                },
                thinking: false,
                stream: false,
                agent_mode: "Agent".to_string(),
                use_tools: vec![],
            },
            event_bus: EventBus::new(),
            state_manager: Arc::new(Mutex::new(StateManager::new())),
            mcp_manager: None,
            hook_manager: Arc::new(HookManager::empty()),
        };

        let result = run_with_engine(store.clone(), || async {
            // Inside outer context
            let outer = get_engine_store().unwrap().instance_id.clone();

            // Nested context should override
            let inner_store = EngineStore {
                instance_id: "inner".to_string(),
                ..store.clone()
            };
            run_with_engine(inner_store, || async {
                let inner = get_engine_store().unwrap().instance_id.clone();
                (outer, inner)
            })
            .await
        })
        .await;

        assert_eq!(result, ("outer".to_string(), "inner".to_string()));
    }

    #[tokio::test]
    async fn test_helper_functions() {
        let store = EngineStore {
            instance_id: "test".to_string(),
            working_dir: "/test/dir".to_string(),
            agent_data_dir: "/test/dir/.sema".to_string(),
            core_config: CoreConfig {
                model_profile: ModelProfile {
                    name: "test".to_string(),
                    provider: "test".to_string(),
                    model_name: "test".to_string(),
                    base_url: "http://test".to_string(),
                    api_key: "test".to_string(),
                    max_tokens: 1000,
                    context_length: 4000,
                    adapt: None,
                    vision: None,
                    ..Default::default()
                },
                thinking: false,
                stream: false,
                agent_mode: "Agent".to_string(),
                use_tools: vec![],
            },
            event_bus: EventBus::new(),
            state_manager: Arc::new(Mutex::new(StateManager::new())),
            mcp_manager: None,
            hook_manager: Arc::new(HookManager::empty()),
        };

        let result = run_with_engine(store, || async {
            (
                get_working_dir(),
                get_agent_data_dir(),
                get_model_profile().map(|p| p.name),
            )
        })
        .await;

        assert_eq!(
            result,
            (
                Some("/test/dir".to_string()),
                Some("/test/dir/.sema".to_string()),
                Some("test".to_string())
            )
        );
    }
}

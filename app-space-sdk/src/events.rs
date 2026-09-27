use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

type Listener = Box<dyn Fn() + Send + Sync>;

/// A simple thread-safe EventEmitter mimicking Node.js `events.EventEmitter`.
#[derive(Clone)]
pub struct EventEmitter {
    listeners: Arc<RwLock<HashMap<String, Vec<Arc<Listener>>>>>,
}

impl Default for EventEmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl EventEmitter {
    pub fn new() -> Self {
        Self {
            listeners: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a listener for a given event name.
    pub async fn on<F>(&self, event: &str, listener: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        let mut map = self.listeners.write().await;
        let entry = map.entry(event.to_string()).or_insert_with(Vec::new);
        entry.push(Arc::new(Box::new(listener)));
    }

    /// Emit an event, calling all registered listeners asynchronously.
    pub async fn emit(&self, event: &str) {
        let map = self.listeners.read().await;
        if let Some(listeners) = map.get(event) {
            for listener in listeners {
                listener();
            }
        }
    }
}

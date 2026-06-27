use crate::errors::DaemonError;
use std::collections::HashMap;

/// Capability a handler may request for a bot.
pub type Capability = String;

/// Reference to a registered handler.
#[derive(Debug, Clone)]
pub struct HandlerRef {
    pub id: String,
    pub bot_ids: Vec<String>,
    pub event_types: Vec<String>,
    pub capabilities: Vec<Capability>,
}

/// Registry of active handler connections.
#[derive(Debug, Default)]
pub struct HandlerRegistry {
    handlers: HashMap<String, HandlerRef>,
}

impl HandlerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, handler: HandlerRef) -> Result<String, DaemonError> {
        let id = handler.id.clone();
        self.handlers.insert(id.clone(), handler);
        Ok(id)
    }

    pub fn unregister(&mut self, handler_id: &str) -> Result<(), DaemonError> {
        self.handlers.remove(handler_id);
        Ok(())
    }

    pub fn get_handler(&self, handler_id: &str) -> Option<&HandlerRef> {
        self.handlers.get(handler_id)
    }

    pub fn is_authorized(&self, handler_id: &str, bot_id: &str) -> bool {
        self.handlers
            .get(handler_id)
            .is_some_and(|h| h.bot_ids.contains(&bot_id.to_string()))
    }
}

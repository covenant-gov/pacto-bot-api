use crate::errors::DaemonError;
use crate::events::AgentEvent;

/// Placeholder event dispatch router.
#[derive(Debug, Default)]
pub struct Dispatch;

impl Dispatch {
    pub fn new() -> Self {
        Self
    }

    pub async fn dispatch(&self, _event: AgentEvent) -> Result<(), DaemonError> {
        Ok(())
    }
}

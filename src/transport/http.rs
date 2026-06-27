use crate::errors::DaemonError;

/// Placeholder localhost HTTP transport.
#[derive(Debug)]
pub struct HttpTransport {
    pub bind: String,
}

impl HttpTransport {
    pub fn new(bind: impl Into<String>) -> Self {
        Self { bind: bind.into() }
    }

    pub async fn run(&self) -> Result<(), DaemonError> {
        Ok(())
    }
}

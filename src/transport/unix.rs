use crate::errors::DaemonError;

/// Placeholder Unix domain socket transport.
#[derive(Debug)]
pub struct UnixTransport {
    pub socket_path: String,
}

impl UnixTransport {
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub async fn run(&self) -> Result<(), DaemonError> {
        Ok(())
    }
}

use crate::errors::DaemonError;
use crate::handlers::ConnectionHandle;
use crate::transport::MessageHandler;
use crate::transport::protocol::{
    JsonRpcMessage, MAX_FRAME_BYTES, parse_message, serialize_message,
};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc, oneshot};

/// Unix domain socket transport for JSON-RPC handlers.
#[derive(Debug)]
pub struct UnixTransport {
    socket_path: PathBuf,
    max_frame_size: usize,
    idle_timeout: Duration,
    max_connections: usize,
}

impl UnixTransport {
    /// Create a new Unix transport bound to `socket_path`.
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            max_frame_size: MAX_FRAME_BYTES,
            idle_timeout: Duration::from_secs(300),
            max_connections: 128,
        }
    }

    /// Override the default resource limits.
    pub fn with_limits(
        mut self,
        max_frame_size: usize,
        idle_timeout: Duration,
        max_connections: usize,
    ) -> Self {
        self.max_frame_size = max_frame_size;
        self.idle_timeout = idle_timeout;
        self.max_connections = max_connections;
        self
    }

    /// Bind the socket, accept connections, and forward messages to `handler`.
    ///
    /// Runs until `shutdown` fires or an accept error occurs.
    pub async fn run(
        self,
        handler: MessageHandler,
        disconnect_tx: mpsc::Sender<Option<String>>,
        shutdown: oneshot::Receiver<()>,
    ) -> Result<(), DaemonError> {
        ensure_socket_directory(&self.socket_path).await?;
        remove_stale_socket(&self.socket_path).await?;

        let listener = UnixListener::bind(&self.socket_path).map_err(|e| {
            DaemonError::Config(format!(
                "failed to bind unix socket {}: {e}",
                self.socket_path.display()
            ))
        })?;

        set_socket_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))?;

        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(self.max_connections));
        let mut shutdown = shutdown;

        loop {
            tokio::select! {
                _ = &mut shutdown => break Ok(()),
                res = listener.accept() => {
                    let (stream, _) = res?;
                    let permit = match semaphore.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            // At connection limit; close the new connection immediately.
                            continue;
                        }
                    };
                    let handler = handler.clone();
                    let disconnect_tx = disconnect_tx.clone();
                    let max_frame_size = self.max_frame_size;
                    let idle_timeout = self.idle_timeout;
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _ = handle_connection(
                            stream,
                            handler,
                            disconnect_tx,
                            max_frame_size,
                            idle_timeout,
                        )
                        .await;
                    });
                }
            }
        }
    }
}

impl Drop for UnixTransport {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

async fn remove_stale_socket(path: &Path) -> Result<(), DaemonError> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(DaemonError::Io(e)),
    };

    if metadata.file_type().is_socket() {
        // If the socket is live, refuse to steal it.
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Err(DaemonError::Config(format!(
                "unix socket {} is already in use",
                path.display()
            )));
        }
    }

    tokio::fs::remove_file(path).await?;
    Ok(())
}

fn set_socket_permissions(
    path: &Path,
    permissions: std::fs::Permissions,
) -> Result<(), DaemonError> {
    #[cfg(unix)]
    {
        std::fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        let _ = permissions;
    }
    Ok(())
}

/// Create the parent directory for the Unix socket with owner-only
/// permissions (0o700) if it does not already exist, and tighten overly
/// permissive directories to 0o700.
async fn ensure_socket_directory(socket_path: &Path) -> Result<(), DaemonError> {
    let Some(parent) = socket_path.parent() else {
        return Ok(());
    };

    match tokio::fs::metadata(parent).await {
        Ok(metadata) => {
            #[cfg(unix)]
            {
                let mode = metadata.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                        .await?;
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(parent).await?;
            #[cfg(unix)]
            {
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
            }
        }
        Err(e) => return Err(DaemonError::Io(e)),
    }

    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    handler: MessageHandler,
    disconnect_tx: mpsc::Sender<Option<String>>,
    max_frame_size: usize,
    idle_timeout: Duration,
) -> Result<(), DaemonError> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();

    // Bounded outbound buffer: responses await room (backpressure on a slow
    // peer), while async notifications are dropped when the buffer is full so
    // the dispatcher never blocks on a non-reading handler.
    const OUTBOUND_BUFFER: usize = 128;
    let (out_tx, mut out_rx) = mpsc::channel::<JsonRpcMessage>(OUTBOUND_BUFFER);
    let connection = ConnectionHandle::with_transport(out_tx.clone(), "unix");
    let handler_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Writer task: forwards outbound messages to the socket.
    let writer_handle = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(msg) = out_rx.recv().await {
            if write_message(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    // Run the read loop in a scoped async block so `out_tx` is dropped before
    // we await the writer task. Otherwise a connection teardown can hang the
    // writer, which is blocked waiting for outbound messages.
    let handler_id_for_loop = Arc::clone(&handler_id);
    let result = async move {
        loop {
            buf.clear();
            let read_future = reader.read_until(b'\n', &mut buf);
            let n = match tokio::time::timeout(idle_timeout, read_future).await {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(DaemonError::Io(e)),
                Err(_) => return Ok(()),
            };

            if n == 0 {
                // Peer closed the connection cleanly.
                return Ok(());
            }

            if buf.len() > max_frame_size {
                // Oversized frame: drop the connection per R3.
                return Ok(());
            }

            // Strip the trailing newline for parsing.
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            if buf.is_empty() {
                continue;
            }

            let line = String::from_utf8(buf.clone())
                .map_err(|_| DaemonError::Config("frame is not valid UTF-8".into()))?;

            let response = match parse_message(&line) {
                Ok(msg) => {
                    let id = msg.id().cloned();
                    let current_handler_id = handler_id_for_loop.lock().await.clone();
                    match handler(msg, connection.clone(), current_handler_id).await {
                        Ok(resp) => resp,
                        Err(e) => id.map(|id| JsonRpcMessage::error(id, e.into())),
                    }
                }
                Err(e) => Some(JsonRpcMessage::error(serde_json::Value::Null, e.into())),
            };

            if let Some(JsonRpcMessage::Response {
                result: Some(r), ..
            }) = &response
            {
                // If this is a successful handler.register response, remember the
                // handler id so subsequent calls on this connection are authorized.
                if let Some(id) = r.get("handler_id").and_then(|v| v.as_str()) {
                    *handler_id_for_loop.lock().await = Some(id.to_string());
                }
            }

            if let Some(resp) = response
                && out_tx.send(resp).await.is_err()
            {
                return Ok(());
            }
        }
    }
    .await;

    // Notify dispatch that this connection has ended so the handler
    // registration (if any) can be removed. Do this before awaiting the
    // writer task: the registry may hold the last outbound sender clone,
    // and unregistering is what allows the writer to shut down.
    let final_handler_id = handler_id.lock().await.clone();
    let _ = disconnect_tx.send(final_handler_id).await;

    let _ = writer_handle.await;

    result
}

async fn write_message(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    msg: &JsonRpcMessage,
) -> Result<(), std::io::Error> {
    let line = serialize_message(msg).map_err(|e| std::io::Error::other(e.to_string()))?;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

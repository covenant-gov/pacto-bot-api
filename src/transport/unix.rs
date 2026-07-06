use crate::errors::DaemonError;
use crate::handlers::ConnectionHandle;
use crate::transport::BoxFuture;
use crate::transport::MessageHandler;
use crate::transport::protocol::{
    JsonRpcMessage, MAX_FRAME_BYTES, parse_message, serialize_message, validate_params,
};
use async_trait::async_trait;
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc, oneshot};

/// Trait abstracting Unix listener accept so the loop can be tested with a
/// fake acceptor that injects errors.
#[async_trait]
trait UnixAcceptor: Send + Sync {
    async fn accept(&self) -> Result<UnixStream, std::io::Error>;
}

#[async_trait]
impl UnixAcceptor for UnixListener {
    async fn accept(&self) -> Result<UnixStream, std::io::Error> {
        self.accept().await.map(|(stream, _)| stream)
    }
}

/// Return true when an accept error indicates the listener is no longer usable.
fn is_fatal_accept_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotConnected | std::io::ErrorKind::BrokenPipe
    )
}

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
    /// Runs until `shutdown` fires or a listener-fatal accept error occurs.
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

        set_socket_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600)).await?;

        self.run_listener(&listener, handler, disconnect_tx, shutdown)
            .await
    }

    /// Accept and serve connections until shutdown or a fatal listener error.
    async fn run_listener(
        &self,
        acceptor: &impl UnixAcceptor,
        handler: MessageHandler,
        disconnect_tx: mpsc::Sender<Option<String>>,
        mut shutdown: oneshot::Receiver<()>,
    ) -> Result<(), DaemonError> {
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(self.max_connections));

        loop {
            tokio::select! {
                _ = &mut shutdown => break Ok(()),
                res = acceptor.accept() => {
                    let stream = match res {
                        Ok(stream) => stream,
                        Err(e) => {
                            if is_fatal_accept_error(&e) {
                                return Err(DaemonError::Io(e));
                            }
                            tracing::warn!(error = %e, "Unix accept error; continuing");
                            continue;
                        }
                    };
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
        let path = self.socket_path.clone();
        if let Ok(_handle) = tokio::runtime::Handle::try_current() {
            std::mem::drop(tokio::task::spawn_blocking(move || {
                let _ = std::fs::remove_file(&path);
            }));
        } else {
            let _ = std::fs::remove_file(&path);
        }
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

async fn set_socket_permissions(
    path: &Path,
    permissions: std::fs::Permissions,
) -> Result<(), DaemonError> {
    #[cfg(unix)]
    {
        tokio::fs::set_permissions(path, permissions).await?;
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
    let (pending_tx, mut pending_rx) =
        mpsc::channel::<BoxFuture<Option<JsonRpcMessage>>>(OUTBOUND_BUFFER);
    let connection = ConnectionHandle::with_transport(out_tx.clone(), "unix");
    let handler_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let writer_handle = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(msg) = out_rx.recv().await {
            if write_message(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });
    let writer_abort = writer_handle.abort_handle();

    // Sequential response drainer: processes one queued response future at a
    // time in arrival order. This preserves connection-level side effects
    // (e.g., handler.register creating a handler_id) while the read loop can
    // continue reading frames and queueing futures.
    let handler_id_for_drain = Arc::clone(&handler_id);
    let out_tx_for_drain = out_tx.clone();
    let drain_handle = tokio::spawn(async move {
        while let Some(fut) = pending_rx.recv().await {
            let response = fut.await;

            if let Some(JsonRpcMessage::Response {
                result: Some(r), ..
            }) = &response
                && let Some(id) = r.get("handler_id").and_then(|v| v.as_str())
            {
                *handler_id_for_drain.lock().await = Some(id.to_string());
            }

            if let Some(resp) = response
                && out_tx_for_drain.send(resp).await.is_err()
            {
                break;
            }
        }
    });
    let drain_abort = drain_handle.abort_handle();

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

            let handler = Arc::clone(&handler);
            let handler_id_for_message = Arc::clone(&handler_id_for_loop);
            let connection = connection.clone();
            let fut = Box::pin(async move {
                match parse_message(&line) {
                    Ok(msg) => {
                        let id = msg.id().cloned();
                        let current_handler_id = handler_id_for_message.lock().await.clone();

                        // Runtime OpenRPC schema validation of incoming params.
                        if let Some(name) = msg.method()
                            && let Err(e) =
                                validate_params(name, msg.params().unwrap_or(&Value::Null))
                        {
                            return id.map(|id| JsonRpcMessage::error(id, e.into()));
                        }

                        match handler(msg, connection, current_handler_id).await {
                            Ok(resp) => resp,
                            Err(e) => id.map(|id| JsonRpcMessage::error(id, e.into())),
                        }
                    }
                    Err(e) => Some(JsonRpcMessage::error(serde_json::Value::Null, e.into())),
                }
            });

            if pending_tx.send(fut).await.is_err() {
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

    // Abort the drain and writer tasks so the connection tears down even if
    // the registry still holds ConnectionHandle clones that keep the outbound
    // channel open (e.g., tests with a dropped disconnect receiver).
    writer_abort.abort();
    drain_abort.abort();
    let _ = tokio::join!(drain_handle, writer_handle);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MessageHandler;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn ensure_socket_directory_creates_owner_only_parent() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("nested").join("socket.sock");
        ensure_socket_directory(&socket_path).await.unwrap();

        let parent = socket_path.parent().unwrap();
        assert!(parent.is_dir());

        #[cfg(unix)]
        {
            let mode = std::fs::metadata(parent).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[tokio::test]
    async fn remove_stale_socket_deletes_abandoned_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("socket.sock");

        let listener = UnixListener::bind(&socket_path).unwrap();
        drop(listener);

        remove_stale_socket(&socket_path).await.unwrap();
        assert!(!socket_path.exists());
    }

    #[tokio::test]
    async fn remove_stale_socket_ignores_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("does-not-exist.sock");
        remove_stale_socket(&socket_path).await.unwrap();
    }

    #[tokio::test]
    async fn remove_stale_socket_rejects_live_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("socket.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();

        let err = remove_stale_socket(&socket_path).await.unwrap_err();
        assert!(err.to_string().contains("already in use"));
    }

    #[tokio::test]
    async fn socket_helpers_do_not_block_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("a").join("b").join("socket.sock");

        let mut interval = tokio::time::interval(Duration::from_millis(5));
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticks_clone = Arc::clone(&ticks);
        let timer = tokio::spawn(async move {
            for _ in 0..50 {
                interval.tick().await;
                ticks_clone.fetch_add(1, Ordering::SeqCst);
            }
        });

        let path = socket_path.clone();
        let work = tokio::spawn(async move {
            ensure_socket_directory(&path).await.unwrap();
            remove_stale_socket(&path).await.unwrap();
        });

        // The runtime should stay responsive while filesystem work is handled
        // on Tokio's blocking thread pool.
        tokio::time::timeout(
            Duration::from_millis(5),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .unwrap();

        work.await.unwrap();
        timer.await.unwrap();

        let tick_count = ticks.load(Ordering::SeqCst);
        assert!(
            tick_count >= 45,
            "runtime blocked during unix socket setup; only {tick_count} timer ticks fired"
        );
    }

    #[derive(Debug)]
    struct FailingAcceptor {
        listener: UnixListener,
        errors_remaining: AtomicUsize,
        error_kind: std::io::ErrorKind,
        error_sent: tokio::sync::mpsc::Sender<()>,
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl UnixAcceptor for FailingAcceptor {
        async fn accept(&self) -> Result<UnixStream, std::io::Error> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if self
                .errors_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .unwrap_or(0)
                > 0
            {
                let _ = self.error_sent.try_send(());
                return Err(std::io::Error::new(
                    self.error_kind,
                    "simulated accept error",
                ));
            }
            self.listener.accept().await.map(|(stream, _)| stream)
        }
    }

    fn noop_handler() -> MessageHandler {
        Arc::new(|_msg, _conn, _id| Box::pin(async move { Ok::<_, DaemonError>(None) }))
    }

    #[test]
    fn fatal_and_transient_accept_errors_are_distinguished() {
        assert!(!is_fatal_accept_error(&std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "transient",
        )));
        assert!(!is_fatal_accept_error(&std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "transient",
        )));
        assert!(is_fatal_accept_error(&std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "fatal",
        )));
        assert!(is_fatal_accept_error(&std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "fatal",
        )));
    }

    #[tokio::test]
    async fn unix_transport_survives_transient_accept_error() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("socket.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let (error_sent_tx, mut error_sent_rx) = tokio::sync::mpsc::channel(1);
        let acceptor = FailingAcceptor {
            listener,
            errors_remaining: AtomicUsize::new(1),
            error_kind: std::io::ErrorKind::Interrupted,
            error_sent: error_sent_tx,
            call_count: Arc::new(AtomicUsize::new(0)),
        };
        let call_count = Arc::clone(&acceptor.call_count);

        let transport = UnixTransport::new(&socket_path);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (disconnect_tx, _disconnect_rx) = mpsc::channel(1);

        let run_handle = tokio::spawn(async move {
            transport
                .run_listener(&acceptor, noop_handler(), disconnect_tx, shutdown_rx)
                .await
        });

        // Wait until the loop has processed the transient accept error.
        error_sent_rx.recv().await.unwrap();
        let count_after_error = call_count.load(Ordering::SeqCst);
        assert!(
            count_after_error >= 1,
            "expected at least one accept attempt before transient error"
        );

        // The loop should still be running and exit cleanly on shutdown.
        let _ = shutdown_tx.send(());
        let result = tokio::time::timeout(Duration::from_secs(5), run_handle)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_ok());
        assert!(
            call_count.load(Ordering::SeqCst) >= 2,
            "expected loop to continue after transient error"
        );
    }

    #[tokio::test]
    async fn unix_transport_fatal_accept_error_stops_listener() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("socket.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let (error_sent_tx, _error_sent_rx) = tokio::sync::mpsc::channel(1);
        let acceptor = FailingAcceptor {
            listener,
            errors_remaining: AtomicUsize::new(1),
            error_kind: std::io::ErrorKind::NotConnected,
            error_sent: error_sent_tx,
            call_count: Arc::new(AtomicUsize::new(0)),
        };
        let call_count = Arc::clone(&acceptor.call_count);

        let transport = UnixTransport::new(&socket_path);
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let (disconnect_tx, _disconnect_rx) = mpsc::channel(1);

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            transport.run_listener(&acceptor, noop_handler(), disconnect_tx, shutdown_rx),
        )
        .await
        .unwrap();

        assert!(result.is_err());
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }
}

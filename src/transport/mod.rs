use crate::config::DaemonConfig;
use crate::dispatch::Dispatch;
use crate::errors::DaemonError;
use crate::transport::protocol::JsonRpcMessage;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub mod http;
pub mod protocol;
pub mod unix;

/// A boxed future returned by a message handler.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Handler closure that processes an incoming JSON-RPC message and optionally
/// returns a response.
pub type MessageHandler = Arc<
    dyn Fn(JsonRpcMessage) -> BoxFuture<Result<Option<JsonRpcMessage>, DaemonError>> + Send + Sync,
>;

/// A request routed from a transport to the dispatch sink.
type DispatchRequest = (
    JsonRpcMessage,
    oneshot::Sender<Result<Option<JsonRpcMessage>, DaemonError>>,
);

/// Wrap a closure as a [`MessageHandler`].
pub fn message_handler<F, Fut>(f: F) -> MessageHandler
where
    F: Fn(JsonRpcMessage) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Option<JsonRpcMessage>, DaemonError>> + Send + 'static,
{
    Arc::new(move |msg| Box::pin(f(msg)))
}

/// Combined transport layer exposing JSON-RPC over Unix socket and localhost HTTP.
#[derive(Debug)]
pub struct TransportLayer {
    socket_path: PathBuf,
    http_bind: String,
    data_dir: PathBuf,
    enable_http: bool,
    max_frame_size: usize,
    idle_timeout: Duration,
    max_connections: usize,
}

impl TransportLayer {
    /// Build a transport layer from daemon configuration.
    pub fn new(config: &DaemonConfig, enable_http: bool) -> Self {
        Self {
            socket_path: PathBuf::from(config.socket_path()),
            http_bind: config.daemon.http_bind.clone(),
            data_dir: PathBuf::from(config.data_dir()),
            enable_http,
            max_frame_size: protocol::MAX_FRAME_BYTES,
            idle_timeout: Duration::from_secs(300),
            max_connections: 128,
        }
    }

    /// Start the configured listeners and route incoming messages to `dispatch`.
    ///
    /// The layer runs until `shutdown` fires, then gracefully stops the
    /// transports and the dispatch consumer.
    pub async fn run(
        self,
        dispatch: Arc<Dispatch>,
        shutdown: oneshot::Receiver<()>,
    ) -> Result<(), DaemonError> {
        let (msg_tx, msg_rx) = mpsc::channel::<DispatchRequest>(256);

        let handler = message_handler(move |msg| {
            let tx = msg_tx.clone();
            async move {
                let (resp_tx, resp_rx) = oneshot::channel();
                tx.send((msg, resp_tx))
                    .await
                    .map_err(|_| DaemonError::Config("dispatch sink closed".into()))?;
                resp_rx
                    .await
                    .map_err(|_| DaemonError::Config("dispatch handler dropped".into()))?
            }
        });

        let dispatch_handle = tokio::spawn(dispatch_consumer(dispatch, msg_rx));

        let unix = unix::UnixTransport::new(&self.socket_path).with_limits(
            self.max_frame_size,
            self.idle_timeout,
            self.max_connections,
        );
        let (unix_shutdown_tx, unix_shutdown_rx) = oneshot::channel();
        let handler_for_unix = handler.clone();
        let unix_handle =
            tokio::spawn(async move { unix.run(handler_for_unix, unix_shutdown_rx).await });

        let (http_shutdown_tx, http_shutdown_rx) = oneshot::channel();
        let http_handle: Option<tokio::task::JoinHandle<Result<(), DaemonError>>> =
            if self.enable_http {
                let http = http::HttpTransport::new(&self.http_bind, &self.data_dir)
                    .with_max_frame_size(self.max_frame_size);
                let handler_for_http = handler.clone();
                Some(tokio::spawn(async move {
                    http.run(handler_for_http, http_shutdown_rx).await
                }))
            } else {
                None
            };

        // Drop the last clone of the handler held in this scope so the dispatch
        // channel can close once the transports shut down.
        drop(handler);

        // Wait for the external shutdown signal.
        let _ = shutdown.await;

        let _ = unix_shutdown_tx.send(());
        let _ = http_shutdown_tx.send(());

        let unix_res = unix_handle
            .await
            .map_err(|e| DaemonError::Config(format!("unix transport task panicked: {e}")))?;

        if let Some(handle) = http_handle {
            let http_res = handle
                .await
                .map_err(|e| DaemonError::Config(format!("http transport task panicked: {e}")))?;
            http_res?;
        }

        unix_res?;

        dispatch_handle
            .await
            .map_err(|e| DaemonError::Config(format!("dispatch consumer task panicked: {e}")))?
    }
}

async fn dispatch_consumer(
    dispatch: Arc<Dispatch>,
    mut rx: mpsc::Receiver<DispatchRequest>,
) -> Result<(), DaemonError> {
    while let Some((msg, tx)) = rx.recv().await {
        let resp = dispatch.handle_message(msg, None).await;
        let _ = tx.send(resp);
    }
    Ok(())
}

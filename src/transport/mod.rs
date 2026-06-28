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
pub mod protocol_generated;
pub mod unix;

/// A boxed future returned by a message handler.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Handler closure that processes an incoming JSON-RPC message and optionally
/// returns a response.
///
/// The transport provides an outbound sender so that the dispatch layer can
/// wire live handler connections (e.g. for `agent.event` notifications). The
/// `handler_id` argument is the server-generated id for connections that have
/// already completed `handler.register`; it is `None` before registration.
pub type MessageHandler = Arc<
    dyn Fn(
            JsonRpcMessage,
            mpsc::Sender<JsonRpcMessage>,
            Option<String>,
        ) -> BoxFuture<Result<Option<JsonRpcMessage>, DaemonError>>
        + Send
        + Sync,
>;

/// Events sent from transport connections to the dispatch consumer.
pub(crate) enum TransportEvent {
    /// Route a JSON-RPC message to dispatch and return the response.
    Message(
        JsonRpcMessage,
        Option<mpsc::Sender<JsonRpcMessage>>,
        Option<String>,
        oneshot::Sender<Result<Option<JsonRpcMessage>, DaemonError>>,
    ),
    /// A transport connection ended. The contained handler_id (if any) should
    /// be disconnected (live connection removed, persisted registration kept).
    Disconnect(Option<String>),
}

/// Wrap a closure as a [`MessageHandler`].
pub fn message_handler<F, Fut>(f: F) -> MessageHandler
where
    F: Fn(JsonRpcMessage, mpsc::Sender<JsonRpcMessage>, Option<String>) -> Fut
        + Send
        + Sync
        + 'static,
    Fut: Future<Output = Result<Option<JsonRpcMessage>, DaemonError>> + Send + 'static,
{
    Arc::new(move |msg, out_tx, handler_id| Box::pin(f(msg, out_tx, handler_id)))
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
    http_idle_timeout: Duration,
    http_max_connections: usize,
    http_token: Option<http::HttpToken>,
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
            http_idle_timeout: Duration::from_secs(config.daemon.http_idle_timeout_secs),
            http_max_connections: config.daemon.http_max_connections,
            http_token: None,
        }
    }

    /// Provide an externally managed, reloadable HTTP secret token.
    ///
    /// When set, the HTTP transport uses this handle instead of creating its
    /// own, allowing the daemon to reload the secret at runtime.
    pub fn with_http_token(mut self, token: http::HttpToken) -> Self {
        self.http_token = Some(token);
        self
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
        let (event_tx, event_rx) = mpsc::channel::<TransportEvent>(256);
        let (disconnect_tx, mut disconnect_rx) = mpsc::channel::<Option<String>>(256);

        // Forward transport disconnect notifications into the dispatch event
        // stream so a single consumer can process both messages and disconnects.
        let event_tx_for_disconnect = event_tx.clone();
        tokio::spawn(async move {
            while let Some(handler_id) = disconnect_rx.recv().await {
                if event_tx_for_disconnect
                    .send(TransportEvent::Disconnect(handler_id))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let handler = message_handler(move |msg, out_tx, handler_id| {
            let tx = event_tx.clone();
            async move {
                let (resp_tx, resp_rx) = oneshot::channel();
                tx.send(TransportEvent::Message(
                    msg,
                    Some(out_tx),
                    handler_id,
                    resp_tx,
                ))
                .await
                .map_err(|_| DaemonError::Config("dispatch sink closed".into()))?;
                resp_rx
                    .await
                    .map_err(|_| DaemonError::Config("dispatch handler dropped".into()))?
            }
        });

        let dispatch_handle = tokio::spawn(dispatch_consumer(dispatch, event_rx));

        let unix = unix::UnixTransport::new(&self.socket_path).with_limits(
            self.max_frame_size,
            self.idle_timeout,
            self.max_connections,
        );
        let (unix_shutdown_tx, unix_shutdown_rx) = oneshot::channel();
        let handler_for_unix = handler.clone();
        let disconnect_for_unix = disconnect_tx.clone();
        let unix_handle = tokio::spawn(async move {
            unix.run(handler_for_unix, disconnect_for_unix, unix_shutdown_rx)
                .await
        });

        let (http_shutdown_tx, http_shutdown_rx) = oneshot::channel();
        let http_handle: Option<tokio::task::JoinHandle<Result<(), DaemonError>>> =
            if self.enable_http {
                let mut http = http::HttpTransport::new(&self.http_bind, &self.data_dir)
                    .with_max_frame_size(self.max_frame_size)
                    .with_limits(self.http_max_connections, self.http_idle_timeout);
                if let Some(token) = self.http_token.clone() {
                    http = http.with_token(token);
                }
                let handler_for_http = handler.clone();
                let disconnect_for_http = disconnect_tx.clone();
                Some(tokio::spawn(async move {
                    http.run(handler_for_http, disconnect_for_http, http_shutdown_rx)
                        .await
                }))
            } else {
                None
            };

        // Drop the clones held in this scope so the channels can close once
        // the transports shut down.
        drop(handler);
        drop(disconnect_tx);

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
    mut rx: mpsc::Receiver<TransportEvent>,
) -> Result<(), DaemonError> {
    while let Some(event) = rx.recv().await {
        match event {
            TransportEvent::Message(msg, out_tx, handler_id, resp_tx) => {
                let resp = dispatch
                    .handle_message(msg, handler_id.as_deref(), out_tx)
                    .await;
                let _ = resp_tx.send(resp);
            }
            TransportEvent::Disconnect(Some(handler_id)) => {
                dispatch.disconnect_handler(&handler_id).await;
            }
            TransportEvent::Disconnect(None) => {}
        }
    }
    Ok(())
}

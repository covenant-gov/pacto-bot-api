use crate::errors::{DaemonError, JsonRpcError};
use crate::handlers::ConnectionHandle;
use crate::transport::MessageHandler;
use crate::transport::protocol::{
    JsonRpcMessage, MAX_FRAME_BYTES, Method, parse_method, serialize_message, validate_params,
};
use axum::Router;
use axum::body::Bytes;
use axum::extract::Request;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header::CONTENT_TYPE};
use axum::response::IntoResponse;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use hyper::body::Incoming;
use hyper_util::rt::tokio::TokioTimer;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{Mutex as TokioMutex, RwLock, mpsc, oneshot};
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tower::ServiceExt;
use tracing::info;

const SECRET_HEADER: HeaderName = HeaderName::from_static("x-pacto-bot-secret");
const HANDLER_ID_HEADER: HeaderName = HeaderName::from_static("x-pacto-handler-id");

/// JSON-RPC error code for HTTP requests whose body exceeds `max_frame_size`.
/// Chosen from the next unused Pacto-specific server-error slot to avoid
/// colliding with the existing Bunker error code (`-32003`).
const HTTP_PAYLOAD_TOO_LARGE_CODE: i32 = -32012;

/// Shared, runtime-reloadable HTTP secret token.
pub type HttpToken = Arc<RwLock<SecretString>>;

/// Localhost HTTP transport for JSON-RPC handlers.
#[derive(Debug)]
pub struct HttpTransport {
    bind: String,
    data_dir: PathBuf,
    max_frame_size: usize,
    max_connections: usize,
    idle_timeout: Duration,
    token: Option<HttpToken>,
}

impl HttpTransport {
    /// Create a new HTTP transport.
    pub fn new(bind: impl Into<String>, data_dir: impl AsRef<Path>) -> Self {
        Self {
            bind: bind.into(),
            data_dir: data_dir.as_ref().to_path_buf(),
            max_frame_size: MAX_FRAME_BYTES,
            max_connections: 100,
            idle_timeout: Duration::from_secs(60),
            token: None,
        }
    }

    /// Override the maximum request body size.
    pub fn with_max_frame_size(mut self, max_frame_size: usize) -> Self {
        self.max_frame_size = max_frame_size;
        self
    }

    /// Override the default resource limits.
    pub fn with_limits(mut self, max_connections: usize, idle_timeout: Duration) -> Self {
        self.max_connections = max_connections;
        self.idle_timeout = idle_timeout;
        self
    }

    /// Use an externally managed, reloadable token instead of loading one
    /// from `data_dir/bot_secret_token` when the transport starts.
    pub fn with_token(mut self, token: HttpToken) -> Self {
        self.token = Some(token);
        self
    }

    /// Path to the secret token file.
    pub fn secret_path(&self) -> PathBuf {
        self.data_dir.join("bot_secret_token")
    }

    /// Bind to the configured loopback address and serve JSON-RPC requests.
    ///
    /// Runs until `shutdown` fires or an accept error occurs.
    pub async fn run(
        self,
        handler: MessageHandler,
        disconnect_tx: mpsc::Sender<Option<String>>,
        shutdown: oneshot::Receiver<()>,
    ) -> Result<(), DaemonError> {
        let addr = SocketAddr::from_str(&self.bind).map_err(|e| {
            DaemonError::Config(format!("invalid HTTP bind address {}: {e}", self.bind))
        })?;

        if !addr.ip().is_loopback() {
            return Err(DaemonError::Config(format!(
                "HTTP bind must be loopback-only, got {}",
                self.bind
            )));
        }

        let listener = TcpListener::bind(addr).await?;
        self.run_with_listener(listener, handler, disconnect_tx, shutdown)
            .await
    }

    /// Serve JSON-RPC requests on an already-bound loopback listener.
    ///
    /// Useful in tests that need to know the ephemeral port.
    pub async fn run_with_listener(
        self,
        listener: TcpListener,
        handler: MessageHandler,
        disconnect_tx: mpsc::Sender<Option<String>>,
        shutdown: oneshot::Receiver<()>,
    ) -> Result<(), DaemonError> {
        let addr = listener.local_addr().map_err(|e| {
            DaemonError::Config(format!("failed to read listener local address: {e}"))
        })?;
        if !addr.ip().is_loopback() {
            return Err(DaemonError::Config(format!(
                "HTTP listener must be loopback-only, got {}",
                addr
            )));
        }

        let token = match self.token {
            Some(token) => token,
            None => Arc::new(RwLock::new(load_or_create_token(&self.data_dir).await?)),
        };

        let state = AppState {
            handler,
            token,
            max_frame_size: self.max_frame_size,
            outbound: Arc::new(TokioMutex::new(HashMap::new())),
            disconnect_tx,
        };

        info!(addr = %addr, "localhost HTTP transport bound");

        let app = Router::new()
            .route("/", post(http_handler))
            .route("/version", get(version_handler))
            .route("/events", get(events_handler))
            .with_state(state);

        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.max_connections));
        let mut shutdown = shutdown;

        loop {
            tokio::select! {
                _ = &mut shutdown => break Ok(()),
                res = listener.accept() => {
                    let (stream, _) = match res {
                        Ok(pair) => pair,
                        Err(e) => {
                            tracing::warn!(error = %e, "HTTP accept error; backing off");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    };

                    let permit = match semaphore.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            tokio::spawn(async move {
                                reject_connection(stream).await;
                            });
                            continue;
                        }
                    };

                    let app = app.clone();
                    let idle_timeout = self.idle_timeout;
                    tokio::spawn(async move {
                        let _permit = permit;
                        let io = TokioIo::new(stream);
                        let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                            app.clone().oneshot(request)
                        });

                        let mut builder = server::conn::auto::Builder::new(TokioExecutor::new());
                        builder.http1().timer(TokioTimer::new());
                        builder.http1().header_read_timeout(idle_timeout);

                        if let Err(err) = builder.serve_connection(io, service).await {
                            tracing::debug!(error = %err, "HTTP connection closed with error");
                        }
                    });
                }
            }
        }
    }
}

async fn reject_connection(mut stream: tokio::net::TcpStream) {
    let response = b"HTTP/1.1 503 Service Unavailable\r\n\
                      Content-Length: 0\r\n\
                      Connection: close\r\n\r\n";
    let _ = stream.write_all(response).await;
}

type OutboundMap = Arc<TokioMutex<HashMap<String, tokio::sync::mpsc::Receiver<JsonRpcMessage>>>>;

#[derive(Clone)]
struct AppState {
    handler: MessageHandler,
    token: HttpToken,
    max_frame_size: usize,
    /// Channels created during `handler.register` that the SSE endpoint can
    /// take over to stream daemon-to-handler notifications.
    outbound: OutboundMap,
    /// Notifies the dispatch consumer when an SSE stream ends.
    disconnect_tx: mpsc::Sender<Option<String>>,
}

/// Result of processing a single JSON-RPC message.
enum SingleMessageResult {
    /// A successful or error JSON-RPC response to return to the client.
    Response(Option<JsonRpcMessage>),
    /// The message triggered an HTTP-level error response. The status code
    /// is preserved for single-object requests; batch requests convert
    /// this into a JSON-RPC error object in the response array.
    HttpError(StatusCode, JsonRpcMessage),
}

async fn process_single_message(
    state: AppState,
    headers: &HeaderMap,
    msg: JsonRpcMessage,
) -> SingleMessageResult {
    let id = msg.id().cloned();
    let method_name = msg.method().map(|s: &str| s.to_string());
    let method = method_name.as_deref().and_then(|m| parse_method(m).ok());
    let is_notification = msg.id().is_none();

    // Runtime OpenRPC schema validation of incoming params.
    if let Some(name) = msg.method()
        && let Err(e) = validate_params(name, msg.params().unwrap_or(&Value::Null))
    {
        if is_notification {
            // Notifications do not receive responses, even for validation errors.
            return SingleMessageResult::Response(None);
        }
        let err = JsonRpcMessage::error(id.unwrap_or(Value::Null), e.into());
        return SingleMessageResult::HttpError(StatusCode::BAD_REQUEST, err);
    }

    if method == Some(Method::HandlerRegister) || method == Some(Method::HandlerReconnect) {
        let response = handle_register(state, msg, id).await;
        return if is_notification {
            SingleMessageResult::Response(None)
        } else {
            SingleMessageResult::Response(response)
        };
    }

    let handler_id = headers
        .get(&HANDLER_ID_HEADER)
        .and_then(|h| h.to_str().ok());
    // Mutating methods require a per-request handler identity because HTTP
    // has no per-connection registration state.
    if handler_id.is_none() && is_mutating_method(method) {
        if is_notification {
            return SingleMessageResult::Response(None);
        }
        let err = JsonRpcMessage::error(
            id.unwrap_or(Value::Null),
            JsonRpcError::new(-32006, "handler identity required"),
        );
        return SingleMessageResult::HttpError(StatusCode::UNAUTHORIZED, err);
    }

    // Non-registration requests do not need a persistent outbound channel,
    // so we pass a disconnected handle.
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let connection = ConnectionHandle::new(tx);
    let handler_id_owned = handler_id.map(|s| s.to_string());
    let response = match (state.handler)(msg, connection, handler_id_owned).await {
        Ok(resp) => resp,
        Err(e) => id.map(|id| JsonRpcMessage::error(id, e.into())),
    };

    if is_notification {
        SingleMessageResult::Response(None)
    } else {
        SingleMessageResult::Response(response)
    }
}

async fn version_handler() -> impl IntoResponse {
    let body = serde_json::json!({
        "version": crate::version::VERSION,
        "git_sha": crate::version::GIT_COMMIT_SHORT,
    });
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "application/json; charset=utf-8")],
        body.to_string().into_bytes(),
    )
}

async fn http_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let token = state.token.read().await;
    if !verify_secret(&headers, &token) {
        let err = JsonRpcMessage::error(Value::Null, JsonRpcError::new(-32000, "unauthorized"));
        let mut body = serialize_message(&err).unwrap_or_default();
        body.push('\n');
        return (
            StatusCode::UNAUTHORIZED,
            [(CONTENT_TYPE, "application/json; charset=utf-8")],
            body.into_bytes(),
        );
    }
    drop(token);

    if body.len() > state.max_frame_size {
        let err = JsonRpcMessage::error(
            Value::Null,
            JsonRpcError::new(HTTP_PAYLOAD_TOO_LARGE_CODE, "payload too large"),
        );
        let mut body = serialize_message(&err).unwrap_or_default();
        body.push('\n');
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            [(CONTENT_TYPE, "application/json; charset=utf-8")],
            body.into_bytes(),
        );
    }

    let text = match String::from_utf8(body.to_vec()) {
        Ok(text) => text,
        Err(_) => {
            let err = JsonRpcMessage::error(
                Value::Null,
                JsonRpcError::new(-32700, "body is not valid UTF-8"),
            );
            let mut body = serialize_message(&err).unwrap_or_default();
            body.push('\n');
            return (
                StatusCode::BAD_REQUEST,
                [(CONTENT_TYPE, "application/json; charset=utf-8")],
                body.into_bytes(),
            );
        }
    };

    let value = match serde_json::from_str::<Value>(&text) {
        Ok(v) => v,
        Err(e) => {
            let err = JsonRpcMessage::error(
                Value::Null,
                JsonRpcError::new(-32700, format!("parse error: {e}")),
            );
            let mut body = serialize_message(&err).unwrap_or_default();
            body.push('\n');
            return (
                StatusCode::BAD_REQUEST,
                [(CONTENT_TYPE, "application/json; charset=utf-8")],
                body.into_bytes(),
            );
        }
    };

    match value {
        Value::Array(items) => {
            if items.is_empty() {
                let err = JsonRpcMessage::error(
                    Value::Null,
                    JsonRpcError::new(-32600, "invalid request: empty batch"),
                );
                let mut body = serialize_message(&err).unwrap_or_default();
                body.push('\n');
                return (
                    StatusCode::BAD_REQUEST,
                    [(CONTENT_TYPE, "application/json; charset=utf-8")],
                    body.into_bytes(),
                );
            }
            let mut responses = Vec::new();
            for item in items {
                match serde_json::from_value::<JsonRpcMessage>(item) {
                    Ok(msg) => match process_single_message(state.clone(), &headers, msg).await {
                        SingleMessageResult::Response(resp) => {
                            if let Some(resp) = resp {
                                responses.push(resp);
                            }
                        }
                        SingleMessageResult::HttpError(_, err) => {
                            responses.push(err);
                        }
                    },
                    Err(e) => {
                        responses.push(JsonRpcMessage::error(
                            Value::Null,
                            JsonRpcError::new(-32600, e.to_string()),
                        ));
                    }
                }
            }
            let body = serde_json::to_string(&responses).unwrap_or_default();
            (
                StatusCode::OK,
                [(CONTENT_TYPE, "application/json; charset=utf-8")],
                body.into_bytes(),
            )
        }
        _ => {
            let msg = match serde_json::from_value::<JsonRpcMessage>(value) {
                Ok(msg) => msg,
                Err(e) => {
                    let err = JsonRpcMessage::error(
                        Value::Null,
                        JsonRpcError::new(-32600, e.to_string()),
                    );
                    let mut body = serialize_message(&err).unwrap_or_default();
                    body.push('\n');
                    return (
                        StatusCode::BAD_REQUEST,
                        [(CONTENT_TYPE, "application/json; charset=utf-8")],
                        body.into_bytes(),
                    );
                }
            };

            match process_single_message(state, &headers, msg).await {
                SingleMessageResult::Response(resp) => {
                    let mut body = resp
                        .as_ref()
                        .and_then(|r| serialize_message(r).ok())
                        .unwrap_or_default();
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    (
                        StatusCode::OK,
                        [(CONTENT_TYPE, "application/json; charset=utf-8")],
                        body.into_bytes(),
                    )
                }
                SingleMessageResult::HttpError(status, err) => {
                    let mut body = serialize_message(&err).unwrap_or_default();
                    body.push('\n');
                    (
                        status,
                        [(CONTENT_TYPE, "application/json; charset=utf-8")],
                        body.into_bytes(),
                    )
                }
            }
        }
    }
}

async fn handle_register(
    state: AppState,
    msg: JsonRpcMessage,
    id: Option<Value>,
) -> Option<JsonRpcMessage> {
    let (out_tx, out_rx) = tokio::sync::mpsc::channel(64);
    let connection = ConnectionHandle::with_transport(out_tx.clone(), "http");

    let response = match (state.handler)(msg, connection, None).await {
        Ok(resp) => resp,
        Err(e) => id.map(|id| JsonRpcMessage::error(id, e.into())),
    };

    if let Some(JsonRpcMessage::Response {
        result: Some(result),
        ..
    }) = &response
        && let Some(handler_id) = result.get("handler_id").and_then(Value::as_str)
    {
        state
            .outbound
            .lock()
            .await
            .insert(handler_id.to_string(), out_rx);
    }

    response
}

/// SSE stream that notifies the dispatch consumer when the client disconnects.
struct NotifyingReceiverStream {
    inner: ReceiverStream<JsonRpcMessage>,
    handler_id: String,
    disconnect_tx: Option<mpsc::Sender<Option<String>>>,
}

impl Stream for NotifyingReceiverStream {
    type Item = Result<SseEvent, std::convert::Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(msg)) => {
                let event_type = msg.method().unwrap_or("message").to_string();
                let data = serialize_message(&msg).unwrap_or_default();
                Poll::Ready(Some(Ok(SseEvent::default().event(event_type).data(data))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for NotifyingReceiverStream {
    fn drop(&mut self) {
        if let Some(tx) = self.disconnect_tx.take() {
            let _ = tx.try_send(Some(self.handler_id.clone()));
        }
    }
}

async fn events_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> impl IntoResponse {
    let token = state.token.read().await;
    if !verify_secret(&headers, &token) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    let header_handler_id = headers
        .get(&HANDLER_ID_HEADER)
        .and_then(|h| h.to_str().ok());
    if header_handler_id.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            "x-pacto-handler-id header required",
        )
            .into_response();
    }
    if header_handler_id != Some(query.handler_id.as_str()) {
        return (StatusCode::FORBIDDEN, "handler_id mismatch").into_response();
    }

    let mut outbound = state.outbound.lock().await;
    let receiver = match outbound.remove(&query.handler_id) {
        Some(rx) => rx,
        None => {
            return (StatusCode::NOT_FOUND, "handler not registered").into_response();
        }
    };

    let stream = NotifyingReceiverStream {
        inner: ReceiverStream::new(receiver),
        handler_id: query.handler_id,
        disconnect_tx: Some(state.disconnect_tx.clone()),
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::new())
        .into_response()
}

#[derive(Deserialize)]
struct EventsQuery {
    handler_id: String,
}

/// Returns true for methods that mutate daemon or bot state and therefore
/// require a registered handler identity over the stateless HTTP transport.
fn is_mutating_method(method: Option<Method>) -> bool {
    matches!(
        method,
        Some(Method::HandlerUnregister)
            | Some(Method::AgentSendDm)
            | Some(Method::AgentSetProfile)
            | Some(Method::AgentError)
            | Some(Method::AgentListHandlers)
            | Some(Method::AgentUnregisterHandler)
            | Some(Method::AgentSendGroupMessage)
            | Some(Method::AgentPublishKeyPackage)
            | Some(Method::AgentCreateMlsGroup)
            | Some(Method::AgentInviteToMlsGroup)
    )
}

fn verify_secret(headers: &HeaderMap, token: &SecretString) -> bool {
    let Some(header) = headers.get(&SECRET_HEADER) else {
        return false;
    };
    let Ok(provided) = header.to_str() else {
        return false;
    };
    let expected = token.expose_secret().as_bytes();
    let provided = provided.as_bytes();

    // Compare in constant time without short-circuiting on length mismatch.
    // The loop always runs for the expected secret length so that timing
    // does not reveal whether the provided token had the correct length.
    let mut result = expected.len().ct_eq(&provided.len());
    for (i, e) in expected.iter().enumerate() {
        let p = provided.get(i).copied().unwrap_or(0);
        result &= e.ct_eq(&p);
    }
    bool::from(result)
}

async fn load_or_create_token(data_dir: &Path) -> Result<SecretString, DaemonError> {
    let path = data_dir.join("bot_secret_token");

    match tokio::fs::metadata(&path).await {
        Ok(_metadata) => {
            #[cfg(unix)]
            {
                let mode = _metadata.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    return Err(DaemonError::Config(format!(
                        "HTTP secret token file {} has overly permissive mode {:03o}; expected 0o600 or stricter",
                        path.display(),
                        mode
                    )));
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(data_dir).await?;

            let path = path.to_path_buf();
            let data_dir = data_dir.to_path_buf();
            let token = tokio::task::spawn_blocking(move || -> Result<String, DaemonError> {
                let mut bytes = [0u8; 32];
                getrandom::getrandom(&mut bytes)
                    .map_err(|e| DaemonError::Io(std::io::Error::other(e)))?;
                let token = hex::encode(bytes);

                let tmp = data_dir.join("bot_secret_token.tmp");
                // Create the temp file with owner-only permissions from the start
                // so the secret never exists in a group/other-readable state.
                #[cfg(unix)]
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)
                    .map_err(DaemonError::Io)?;
                #[cfg(not(unix))]
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&tmp)
                    .map_err(DaemonError::Io)?;
                let mut file = std::io::BufWriter::new(file);
                file.write_all(token.as_bytes())?;
                file.flush()?;
                drop(file);
                std::fs::rename(&tmp, &path)?;
                Ok(token)
            })
            .await
            .map_err(|e| DaemonError::Config(format!("token creation task failed: {e}")))?;

            let token = token?;
            return Ok(SecretString::new(token.into()));
        }
        Err(e) => return Err(DaemonError::Io(e)),
    }

    let contents = tokio::fs::read_to_string(&path).await?;
    let token = contents.trim().to_string();
    Ok(SecretString::new(token.into()))
}

/// Load or create the HTTP secret token and return it as a shared,
/// runtime-reloadable handle.
pub async fn init_token(data_dir: &Path) -> Result<HttpToken, DaemonError> {
    let secret = load_or_create_token(data_dir).await?;
    Ok(Arc::new(RwLock::new(secret)))
}

/// Load an existing token from `data_dir/bot_secret_token`, enforcing
/// owner-only permissions. Unlike [`load_or_create_token`], this does not
/// create a missing file.
async fn load_token(data_dir: &Path) -> Result<SecretString, DaemonError> {
    let path = data_dir.join("bot_secret_token");

    match tokio::fs::metadata(&path).await {
        Ok(_metadata) => {
            #[cfg(unix)]
            {
                let mode = _metadata.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    return Err(DaemonError::Config(format!(
                        "HTTP secret token file {} has overly permissive mode {:03o}; expected 0o600 or stricter",
                        path.display(),
                        mode
                    )));
                }
            }
        }
        Err(e) => return Err(DaemonError::Io(e)),
    }

    let contents = tokio::fs::read_to_string(&path).await?;
    let token = contents.trim().to_string();
    Ok(SecretString::new(token.into()))
}

/// Re-read the token file and atomically update the in-memory secret.
pub async fn reload_token(token: &HttpToken, data_dir: &Path) -> Result<(), DaemonError> {
    let new_secret = load_token(data_dir).await?;
    let mut guard = token.write().await;
    *guard = new_secret;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn load_or_create_token_does_not_block_runtime() {
        let dir = tempfile::tempdir().unwrap();

        let mut interval = tokio::time::interval(Duration::from_millis(5));
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticks_clone = Arc::clone(&ticks);
        let timer = tokio::spawn(async move {
            for _ in 0..50 {
                interval.tick().await;
                ticks_clone.fetch_add(1, Ordering::SeqCst);
            }
        });

        let dir_path = dir.path().to_path_buf();
        let token = tokio::spawn(async move { load_or_create_token(&dir_path).await.unwrap() });

        // The runtime should remain responsive while the token file is
        // created on a blocking thread.
        tokio::time::timeout(
            Duration::from_millis(5),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .unwrap();

        let token = token.await.unwrap();
        timer.await.unwrap();

        let tick_count = ticks.load(Ordering::SeqCst);
        assert!(
            tick_count >= 45,
            "runtime blocked during token creation; only {tick_count} timer ticks fired"
        );

        let path = dir.path().join("bot_secret_token");
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents.trim(), token.expose_secret());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = tokio::fs::metadata(&path)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}

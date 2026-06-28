use crate::errors::DaemonError;
use crate::transport::MessageHandler;
use crate::transport::protocol::{
    JsonRpcMessage, MAX_FRAME_BYTES, Method, parse_message, parse_method, serialize_message,
};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, StatusCode, header::CONTENT_TYPE};
use axum::response::IntoResponse;
use axum::routing::post;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const SECRET_HEADER: HeaderName = HeaderName::from_static("x-pacto-bot-secret");

/// Localhost HTTP transport for JSON-RPC handlers.
#[derive(Debug)]
pub struct HttpTransport {
    bind: String,
    data_dir: PathBuf,
    max_frame_size: usize,
}

impl HttpTransport {
    /// Create a new HTTP transport.
    pub fn new(bind: impl Into<String>, data_dir: impl AsRef<Path>) -> Self {
        Self {
            bind: bind.into(),
            data_dir: data_dir.as_ref().to_path_buf(),
            max_frame_size: MAX_FRAME_BYTES,
        }
    }

    /// Override the maximum request body size.
    pub fn with_max_frame_size(mut self, max_frame_size: usize) -> Self {
        self.max_frame_size = max_frame_size;
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
        self.run_with_listener(listener, handler, shutdown).await
    }

    /// Serve JSON-RPC requests on an already-bound loopback listener.
    ///
    /// Useful in tests that need to know the ephemeral port.
    pub async fn run_with_listener(
        self,
        listener: TcpListener,
        handler: MessageHandler,
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

        let token = load_or_create_token(&self.data_dir).await?;

        let state = AppState {
            handler,
            token,
            max_frame_size: self.max_frame_size,
        };

        let app = Router::new()
            .route("/", post(http_handler))
            .with_state(state);

        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown.await;
            })
            .await?;

        Ok(())
    }
}

#[derive(Clone)]
struct AppState {
    handler: MessageHandler,
    token: SecretString,
    max_frame_size: usize,
}

async fn http_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !verify_secret(&headers, &state.token) {
        return (
            StatusCode::UNAUTHORIZED,
            [(CONTENT_TYPE, "text/plain")],
            Vec::new(),
        );
    }

    if body.len() > state.max_frame_size {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            [(CONTENT_TYPE, "text/plain")],
            Vec::new(),
        );
    }

    let text = String::from_utf8_lossy(&body);
    let mut responses = Vec::new();

    for line in text.lines() {
        if line.is_empty() {
            continue;
        }

        let response = match parse_message(line) {
            Ok(msg) => {
                let id = msg.id().cloned();
                let method_name = msg.method().map(|s| s.to_string());

                // HTTP is request/response only.  Registration and handler
                // responses require a persistent outbound channel for async
                // daemon→handler notifications; creating them over HTTP would
                // leave a dead registration that dispatch can never reach.
                if matches!(
                    method_name.as_deref().and_then(|m| parse_method(m).ok()),
                    Some(Method::HandlerRegister) | Some(Method::HandlerResponse)
                ) {
                    let err = method_name.map_or_else(
                        || DaemonError::MethodNotFound,
                        DaemonError::MethodNotSupported,
                    );
                    id.map(|id| JsonRpcMessage::error(id, err.into()))
                } else {
                    // No persistent connection over which the daemon can push
                    // async notifications, so we pass a disconnected outbound
                    // sender and no handler id.
                    let (out_tx, _out_rx) = tokio::sync::mpsc::channel(1);
                    match (state.handler)(msg, out_tx, None).await {
                        Ok(resp) => resp,
                        Err(e) => id.map(|id| JsonRpcMessage::error(id, e.into())),
                    }
                }
            }
            Err(e) => Some(JsonRpcMessage::error(Value::Null, e.into())),
        };

        if let Some(resp) = response {
            if let Ok(line) = serialize_message(&resp) {
                responses.push(line);
            }
        }
    }

    let mut body = responses.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }

    (
        StatusCode::OK,
        [(CONTENT_TYPE, "text/plain; charset=utf-8")],
        body.into_bytes(),
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
    if expected.len() != provided.len() {
        return false;
    }
    bool::from(expected.ct_eq(provided))
}

async fn load_or_create_token(data_dir: &Path) -> Result<SecretString, DaemonError> {
    let path = data_dir.join("bot_secret_token");

    match tokio::fs::metadata(&path).await {
        Ok(metadata) => {
            #[cfg(unix)]
            {
                let mode = metadata.permissions().mode() & 0o777;
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
            let mut bytes = [0u8; 32];
            getrandom::getrandom(&mut bytes)
                .map_err(|e| DaemonError::Io(std::io::Error::other(e)))?;
            let token = hex::encode(bytes);

            let tmp = data_dir.join("bot_secret_token.tmp");
            tokio::fs::write(&tmp, token.as_bytes()).await?;
            set_file_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            tokio::fs::rename(&tmp, &path).await?;
        }
        Err(e) => return Err(DaemonError::Io(e)),
    }

    let contents = tokio::fs::read_to_string(&path).await?;
    let token = contents.trim().to_string();
    Ok(SecretString::new(token.into()))
}

fn set_file_permissions(path: &Path, permissions: std::fs::Permissions) -> Result<(), DaemonError> {
    #[cfg(unix)]
    {
        std::fs::set_permissions(path, permissions)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        let _ = permissions;
        Ok(())
    }
}

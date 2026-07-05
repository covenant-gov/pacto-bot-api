//! Minimal JSON-RPC client for the daemon's handler-facing API.
//!
//! Supports both Unix socket (newline-delimited JSON) and localhost HTTP
//! transports. The client is intentionally small: it only implements the three
//! methods the snapshot bot needs: `handler.register`, `agent.publish_key_package`,
//! and `agent.send_group_message`.

use serde_json::Value;
use std::path::Path;

use crate::config::ConfigError;

/// A raw JSON-RPC 2.0 request.
#[derive(Debug, serde::Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: String,
    method: String,
    params: Value,
}

/// A raw JSON-RPC 2.0 response envelope.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

/// Errors that can occur when calling the daemon.
#[derive(Debug, thiserror::Error)]
pub enum DaemonClientError {
    /// Transport-level failure (socket, HTTP, I/O).
    #[error("transport error: {0}")]
    Transport(String),
    /// The daemon returned a JSON-RPC error object.
    #[error("daemon error {code}: {message}")]
    JsonRpc { code: i32, message: String },
    /// The response could not be parsed.
    #[error("decode error: {0}")]
    Decode(String),
    /// Configuration is missing or inconsistent.
    #[error("config error: {0}")]
    Config(String),
}

impl From<ConfigError> for DaemonClientError {
    fn from(err: ConfigError) -> Self {
        DaemonClientError::Config(err.to_string())
    }
}

impl From<serde_json::Error> for DaemonClientError {
    fn from(err: serde_json::Error) -> Self {
        DaemonClientError::Decode(err.to_string())
    }
}

/// Result of registering a handler with the daemon.
#[derive(Debug, Clone)]
pub struct HandlerRegistration {
    pub handler_id: String,
    pub reconnect_token: String,
}

/// Connection to the daemon.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    inner: ClientBackend,
}

#[derive(Debug, Clone)]
enum ClientBackend {
    #[cfg(unix)]
    Unix {
        socket_path: std::path::PathBuf,
    },
    Http {
        url: String,
        secret: String,
    },
}

impl DaemonClient {
    /// Create a client connected over the daemon's Unix socket.
    #[cfg(unix)]
    pub fn unix(socket_path: impl AsRef<Path>) -> Self {
        Self {
            inner: ClientBackend::Unix {
                socket_path: socket_path.as_ref().to_path_buf(),
            },
        }
    }

    /// Create a client connected over the daemon's HTTP transport.
    pub fn http(url: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            inner: ClientBackend::Http {
                url: url.into(),
                secret: secret.into(),
            },
        }
    }

    /// Register this handler for the configured bot and capabilities.
    pub async fn handler_register(
        &self,
        bot_id: &str,
        capabilities: &[&str],
    ) -> Result<HandlerRegistration, DaemonClientError> {
        let params = serde_json::json!({
            "bot_ids": [bot_id],
            "event_types": ["mls_welcome_received"],
            "capabilities": capabilities,
        });
        let result = self.call("handler.register", params).await?;
        let handler_id = result
            .get("handler_id")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonClientError::Decode("handler_id missing".into()))?
            .to_string();
        let reconnect_token = result
            .get("reconnect_token")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonClientError::Decode("reconnect_token missing".into()))?
            .to_string();
        Ok(HandlerRegistration {
            handler_id,
            reconnect_token,
        })
    }

    /// Ask the daemon to publish the bot's MLS KeyPackage.
    pub async fn publish_key_package(&self, bot_id: &str) -> Result<String, DaemonClientError> {
        let params = serde_json::json!({ "bot_id": bot_id });
        let result = self.call("agent.publish_key_package", params).await?;
        result
            .as_str()
            .map(String::from)
            .ok_or_else(|| DaemonClientError::Decode("expected hex event id string".into()))
    }

    /// Ask the daemon to send an encrypted MLS group message.
    pub async fn send_group_message(
        &self,
        bot_id: &str,
        group_id: &str,
        content: &str,
    ) -> Result<String, DaemonClientError> {
        let params = serde_json::json!({
            "bot_id": bot_id,
            "group_id": group_id,
            "content": content,
        });
        let result = self.call("agent.send_group_message", params).await?;
        result
            .as_str()
            .map(String::from)
            .ok_or_else(|| DaemonClientError::Decode("expected hex event id string".into()))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, DaemonClientError> {
        let id = uuid::Uuid::new_v4().to_string();
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id: id.clone(),
            method: method.to_string(),
            params,
        };
        let body = serde_json::to_vec(&request)?;

        match &self.inner {
            #[cfg(unix)]
            ClientBackend::Unix { socket_path } => self.call_unix(socket_path, &body, &id).await,
            ClientBackend::Http { url, secret } => self.call_http(url, secret, &body).await,
        }
    }

    #[cfg(unix)]
    async fn call_unix(
        &self,
        socket_path: &std::path::Path,
        body: &[u8],
        expected_id: &str,
    ) -> Result<Value, DaemonClientError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(|e| DaemonClientError::Transport(format!("unix connect: {e}")))?;
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(body)
            .await
            .map_err(|e| DaemonClientError::Transport(format!("unix write: {e}")))?;
        writer
            .write_all(b"\n")
            .await
            .map_err(|e| DaemonClientError::Transport(format!("unix write: {e}")))?;
        writer
            .flush()
            .await
            .map_err(|e| DaemonClientError::Transport(format!("unix flush: {e}")))?;

        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| DaemonClientError::Transport(format!("unix read: {e}")))?;
        if line.is_empty() {
            return Err(DaemonClientError::Transport("unix socket closed".into()));
        }

        let response: JsonRpcResponse = serde_json::from_str(&line)?;
        if response.id.as_ref().and_then(Value::as_str) != Some(expected_id)
            && response.id.as_ref().and_then(Value::as_str).is_some()
        {
            return Err(DaemonClientError::Decode("response id mismatch".into()));
        }
        if let Some(err) = response.error {
            return Err(DaemonClientError::JsonRpc {
                code: err.code,
                message: err.message,
            });
        }
        response
            .result
            .ok_or_else(|| DaemonClientError::Decode("result missing".into()))
    }

    async fn call_http(
        &self,
        url: &str,
        secret: &str,
        body: &[u8],
    ) -> Result<Value, DaemonClientError> {
        let client = reqwest::Client::new();
        let resp = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-Pacto-Bot-Secret", secret)
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| DaemonClientError::Transport(format!("http request: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| DaemonClientError::Transport(format!("http body: {e}")))?;
        if !status.is_success() {
            return Err(DaemonClientError::Transport(format!(
                "http status {status}: {text}"
            )));
        }

        let response: JsonRpcResponse = serde_json::from_str(&text)?;
        if let Some(err) = response.error {
            return Err(DaemonClientError::JsonRpc {
                code: err.code,
                message: err.message,
            });
        }
        response
            .result
            .ok_or_else(|| DaemonClientError::Decode("result missing".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_client_builds_without_secret_error() {
        let client = DaemonClient::http("http://127.0.0.1:9800", "secret");
        assert!(matches!(client.inner, ClientBackend::Http { .. }));
    }

    #[tokio::test]
    async fn unix_client_reports_connection_error_for_missing_socket() {
        let client = DaemonClient::unix("/nonexistent/path/to/pacto.sock");
        let err = client
            .handler_register("bot", &["SendGroupMessages"])
            .await
            .expect_err("should fail to connect");
        assert!(matches!(err, DaemonClientError::Transport(_)));
    }

    #[test]
    fn json_rpc_error_parses_and_reports_code() {
        let err = DaemonClientError::JsonRpc {
            code: -32600,
            message: "Invalid Request".into(),
        };
        assert_eq!(format!("{err}"), "daemon error -32600: Invalid Request");
    }
}

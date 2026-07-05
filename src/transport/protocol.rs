use crate::errors::{DaemonError, JsonRpcError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::str::FromStr;

/// Maximum size of a single newline-delimited JSON frame (1 MiB).
pub const MAX_FRAME_BYTES: usize = 1_048_576;

fn jsonrpc_version() -> String {
    "2.0".to_string()
}

/// A JSON-RPC 2.0 message on the wire.
///
/// The enum is untagged so that the presence of `method`, `result`, or `error`
/// selects the correct variant while preserving the wire shape for requests,
/// responses, notifications, and error responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    /// A JSON-RPC request that expects a response.
    Request {
        jsonrpc: String,
        id: Value,
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
    /// A JSON-RPC error response.
    ///
    /// Listed before [`Self::Response`] so that untagged deserialization prefers
    /// `error` over the optional `result` field when both could match.
    #[serde(rename = "error")]
    Error {
        jsonrpc: String,
        id: Value,
        error: JsonRpcError,
    },
    /// A JSON-RPC response carrying a successful result.
    Response {
        jsonrpc: String,
        id: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    /// A JSON-RPC notification (no `id`).
    Notification {
        jsonrpc: String,
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
    },
}

impl JsonRpcMessage {
    /// Build a JSON-RPC request.
    pub fn request(id: Value, method: impl Into<String>, params: Option<Value>) -> Self {
        Self::Request {
            jsonrpc: jsonrpc_version(),
            id,
            method: method.into(),
            params,
        }
    }

    /// Build a JSON-RPC notification.
    pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
        Self::Notification {
            jsonrpc: jsonrpc_version(),
            method: method.into(),
            params,
        }
    }

    /// Build a JSON-RPC success response.
    pub fn response(id: Value, result: Option<Value>) -> Self {
        Self::Response {
            jsonrpc: jsonrpc_version(),
            id,
            result,
        }
    }

    /// Build a JSON-RPC error response.
    pub fn error(id: Value, error: JsonRpcError) -> Self {
        Self::Error {
            jsonrpc: jsonrpc_version(),
            id,
            error,
        }
    }

    /// Return the message id, if it has one.
    pub fn id(&self) -> Option<&Value> {
        match self {
            Self::Request { id, .. } | Self::Response { id, .. } | Self::Error { id, .. } => {
                Some(id)
            }
            Self::Notification { .. } => None,
        }
    }

    /// Return the method name, if this is a request or notification.
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request { method, .. } | Self::Notification { method, .. } => Some(method),
            Self::Response { .. } | Self::Error { .. } => None,
        }
    }
}

/// Parse a newline-delimited JSON frame into a [`JsonRpcMessage`].
pub fn parse_message(line: &str) -> Result<JsonRpcMessage, DaemonError> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| DaemonError::JsonRpcParseError(e.to_string()))?;

    let obj = value
        .as_object()
        .ok_or_else(|| DaemonError::InvalidJsonRpcRequest("message is not a JSON object".into()))?;

    match obj.get("jsonrpc") {
        Some(Value::String(s)) if s == "2.0" => {}
        Some(v) => {
            return Err(DaemonError::InvalidJsonRpcRequest(format!(
                "jsonrpc field must be exactly '2.0', got {v}"
            )));
        }
        None => {
            return Err(DaemonError::InvalidJsonRpcRequest(
                "jsonrpc field is missing".into(),
            ));
        }
    }

    serde_json::from_value(value).map_err(|e| DaemonError::InvalidJsonRpcRequest(e.to_string()))
}

/// Validate that `size` does not exceed the maximum frame size.
pub fn validate_frame_size(size: usize) -> Result<(), DaemonError> {
    if size > MAX_FRAME_BYTES {
        Err(DaemonError::FrameTooLarge)
    } else {
        Ok(())
    }
}

/// Parse a byte slice after checking the 1 MB frame-size limit.
pub fn parse_frame(frame: &[u8]) -> Result<JsonRpcMessage, DaemonError> {
    validate_frame_size(frame.len())?;
    let line = std::str::from_utf8(frame)
        .map_err(|_| DaemonError::Config("frame is not valid UTF-8".into()))?;
    parse_message(line)
}

/// Serialize a [`JsonRpcMessage`] to a compact JSON string.
pub fn serialize_message(msg: &JsonRpcMessage) -> Result<String, DaemonError> {
    let s = serde_json::to_string(msg)?;
    Ok(s)
}

/// Known JSON-RPC methods in the daemon catalog.
///
/// This enum is intentionally hand-written rather than generated from
/// `schemas/jsonrpc.json`. The wire representation of [`JsonRpcMessage`] is an
/// untagged enum with helper constructors and frame-parsing behavior that are
/// easier to express and review directly in Rust than through a generic
/// code-generator. `schemas/jsonrpc.json` remains the source of truth for the
/// method catalog and per-method parameter/result schemas; the
/// `tests/schema_sync.rs` gate fails if a method is added to the schema without
/// a matching variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    #[serde(rename = "handler.register")]
    HandlerRegister,
    #[serde(rename = "handler.reconnect")]
    HandlerReconnect,
    #[serde(rename = "handler.unregister")]
    HandlerUnregister,
    #[serde(rename = "agent.send_dm")]
    AgentSendDm,
    #[serde(rename = "agent.set_profile")]
    AgentSetProfile,
    #[serde(rename = "agent.error")]
    AgentError,
    #[serde(rename = "handler.response")]
    HandlerResponse,
    #[serde(rename = "agent.event")]
    AgentEvent,
    #[serde(rename = "agent.status")]
    AgentStatus,
    #[serde(rename = "agent.metrics")]
    AgentMetrics,
    #[serde(rename = "agent.list_handlers")]
    AgentListHandlers,
    #[serde(rename = "agent.unregister_handler")]
    AgentUnregisterHandler,
    #[serde(rename = "agent.version")]
    AgentVersion,
    #[serde(rename = "agent.send_group_message")]
    AgentSendGroupMessage,
    #[serde(rename = "agent.publish_key_package")]
    AgentPublishKeyPackage,
    #[serde(rename = "admin.send_test_dm")]
    AdminSendTestDm,
}

impl Method {
    /// Return every catalog method in a stable order.
    pub const fn all() -> &'static [Method] {
        &[
            Method::HandlerRegister,
            Method::HandlerReconnect,
            Method::HandlerUnregister,
            Method::AgentSendDm,
            Method::AgentSetProfile,
            Method::AgentError,
            Method::HandlerResponse,
            Method::AgentEvent,
            Method::AgentStatus,
            Method::AgentMetrics,
            Method::AgentListHandlers,
            Method::AgentUnregisterHandler,
            Method::AgentVersion,
            Method::AgentSendGroupMessage,
            Method::AgentPublishKeyPackage,
            Method::AdminSendTestDm,
        ]
    }
}

impl FromStr for Method {
    type Err = DaemonError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "handler.register" => Ok(Self::HandlerRegister),
            "handler.reconnect" => Ok(Self::HandlerReconnect),
            "handler.unregister" => Ok(Self::HandlerUnregister),
            "agent.send_dm" => Ok(Self::AgentSendDm),
            "agent.set_profile" => Ok(Self::AgentSetProfile),
            "agent.error" => Ok(Self::AgentError),
            "handler.response" => Ok(Self::HandlerResponse),
            "agent.event" => Ok(Self::AgentEvent),
            "agent.status" => Ok(Self::AgentStatus),
            "agent.metrics" => Ok(Self::AgentMetrics),
            "agent.list_handlers" => Ok(Self::AgentListHandlers),
            "agent.unregister_handler" => Ok(Self::AgentUnregisterHandler),
            "agent.version" => Ok(Self::AgentVersion),
            "agent.send_group_message" => Ok(Self::AgentSendGroupMessage),
            "agent.publish_key_package" => Ok(Self::AgentPublishKeyPackage),
            "admin.send_test_dm" => Ok(Self::AdminSendTestDm),
            _ => Err(DaemonError::MethodNotFound),
        }
    }
}

/// Validate that a method name belongs to the known catalog.
pub fn parse_method(method: &str) -> Result<Method, DaemonError> {
    method.parse()
}

/// Typed payload returned by the `handler.unregister` JSON-RPC method.
///
/// Matches `schemas/jsonrpc.json`: the result is an object with a single
/// `unregistered` boolean field set to `true`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandlerUnregisterResponse {
    /// Always `true` for a successful unregistration.
    pub unregistered: bool,
}

/// Typed payload returned by the `handler.register` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandlerRegisterResponse {
    pub handler_id: String,
    pub reconnect_token: String,
    pub registered_events: Vec<String>,
}

/// Typed payload for the `handler.reconnect` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandlerReconnectParams {
    pub handler_id: String,
    pub reconnect_token: String,
}

/// Typed payload returned by the `handler.reconnect` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandlerReconnectResponse {
    pub handler_id: String,
    pub registered_events: Vec<String>,
}

/// Typed payload returned by the `agent.metrics` JSON-RPC method.
///
/// Matches `schemas/metrics.json` exactly: the schema declares eight
/// non-negative integer counters and no required fields, so every field is
/// optional on the wire. The daemon always populates every counter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricsResponse {
    /// Daemon uptime in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
    /// Number of handlers currently registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handlers_registered: Option<u64>,
    /// Total incoming events accepted by the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_received_total: Option<u64>,
    /// Total events dispatched to handlers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_dispatched_total: Option<u64>,
    /// Total events dropped due to rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limited_total: Option<u64>,
    /// Total relay reconnections observed across all bots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_reconnects_total: Option<u64>,
    /// Total NIP-46 bunker signing failures observed across all bots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bunker_sign_failures_total: Option<u64>,
    /// Total incoming events rejected due to failed signature verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_events_total: Option<u64>,
    /// Total reply DMs that failed to publish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_send_failed_total: Option<u64>,
    /// Events received in the last 10 minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_received_last_10_min: Option<u64>,
    /// Events dispatched in the last 10 minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_dispatched_last_10_min: Option<u64>,
    /// Reply DMs attempted in the last 10 minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replies_last_10_min: Option<u64>,
    /// Reply DMs that failed to publish in the last 10 minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_send_failed_last_10_min: Option<u64>,
    /// Per-bot health summaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bots: Option<Vec<crate::diagnostics::BotHealth>>,
}

impl From<crate::diagnostics::HealthSnapshot> for MetricsResponse {
    fn from(snapshot: crate::diagnostics::HealthSnapshot) -> Self {
        Self {
            uptime_seconds: Some(snapshot.uptime_seconds),
            handlers_registered: Some(snapshot.handlers_registered),
            events_received_total: Some(snapshot.events_received_total),
            events_dispatched_total: Some(snapshot.events_dispatched_total),
            rate_limited_total: Some(snapshot.rate_limited_total),
            relay_reconnects_total: Some(snapshot.relay_reconnects_total),
            bunker_sign_failures_total: Some(snapshot.bunker_sign_failures_total),
            invalid_events_total: Some(snapshot.invalid_events_total),
            reply_send_failed_total: Some(snapshot.reply_send_failed_total),
            events_received_last_10_min: Some(snapshot.recent_counts.events_received),
            events_dispatched_last_10_min: Some(snapshot.recent_counts.events_dispatched),
            replies_last_10_min: Some(snapshot.recent_counts.replies),
            reply_send_failed_last_10_min: Some(snapshot.recent_counts.reply_send_failed),
            bots: Some(snapshot.bots),
        }
    }
}

/// Typed payload returned by the `agent.list_handlers` JSON-RPC method.
///
/// Matches `schemas/jsonrpc.json`: the result is an object containing a
/// `handlers` array with a full row for each registered handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListHandlersEntry {
    pub handler_id: String,
    pub bot_ids: Vec<String>,
    pub event_types: Vec<String>,
    pub capabilities: Vec<String>,
    pub connected: bool,
    pub state: String,
    pub transport: String,
    pub last_seen: String,
    pub registered_at: String,
}

/// Container for the `agent.list_handlers` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListHandlersResponse {
    pub handlers: Vec<AgentListHandlersEntry>,
}

/// Typed payload returned by the `agent.unregister_handler` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentUnregisterHandlerResponse {
    pub unregistered: bool,
}

/// Typed payload returned by the `admin.send_test_dm` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminSendTestDmResponse {
    pub event_id: String,
}

/// Typed payload for the `agent.status` JSON-RPC notification.
///
/// Matches the `params` schema declared in `schemas/jsonrpc.json` for the
/// `agent.status` method: `state` is required, `identity` and `capabilities`
/// are optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatusParams {
    /// Current daemon lifecycle state.
    pub state: String,
    /// Public key of the bot whose state changed, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Capabilities available to the handler.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

/// Typed payload returned by the `agent.version` JSON-RPC method.
///
/// Matches `schemas/version.json`: the result contains the daemon's Cargo
/// package version and the short git commit hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentVersionResponse {
    /// Cargo package version.
    pub version: String,
    /// Short git commit hash (8 characters).
    pub commit: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_serialize_catalog_methods() -> Result<(), DaemonError> {
        let methods = [
            "handler.register",
            "handler.reconnect",
            "handler.unregister",
            "agent.send_dm",
            "agent.set_profile",
            "agent.error",
            "handler.response",
            "agent.event",
            "agent.status",
            "agent.metrics",
            "agent.list_handlers",
            "agent.unregister_handler",
            "agent.version",
            "admin.send_test_dm",
        ];

        for method in methods {
            let req =
                JsonRpcMessage::request(1.into(), method, Some(Value::Object(Default::default())));
            let line = serialize_message(&req)?;
            let parsed = parse_message(&line)?;
            assert_eq!(parsed.method(), Some(method));

            let notif = JsonRpcMessage::notification(method, None);
            let line = serialize_message(&notif)?;
            let parsed = parse_message(&line)?;
            assert_eq!(parsed.method(), Some(method));
            assert!(parsed.id().is_none());
        }

        Ok(())
    }

    #[test]
    fn response_and_error_round_trip() -> Result<(), DaemonError> {
        let resp = JsonRpcMessage::response(42.into(), Some(Value::String("ok".into())));
        let line = serialize_message(&resp)?;
        let parsed = parse_message(&line)?;
        assert_eq!(parsed.id(), Some(&Value::from(42)));

        let err = JsonRpcMessage::error(7.into(), JsonRpcError::new(-32601, "method not found"));
        let line = serialize_message(&err)?;
        let parsed = parse_message(&line)?;
        assert_eq!(parsed.id(), Some(&Value::from(7)));
        Ok(())
    }

    #[test]
    fn frame_size_limit_is_enforced() {
        assert!(validate_frame_size(MAX_FRAME_BYTES).is_ok());
        assert!(validate_frame_size(MAX_FRAME_BYTES + 1).is_err());
    }

    #[test]
    fn unknown_method_is_rejected() {
        assert!(parse_method("not.in.catalog").is_err());
    }

    #[test]
    fn agent_status_params_round_trip() -> Result<(), DaemonError> {
        let params = AgentStatusParams {
            state: "ready".into(),
            identity: Some("npub1test".into()),
            capabilities: vec!["ReadMessages".into(), "SendMessages".into()],
        };
        let msg =
            JsonRpcMessage::notification("agent.status", Some(serde_json::to_value(&params)?));
        let line = serialize_message(&msg)?;
        let parsed = parse_message(&line)?;
        assert_eq!(parsed.method(), Some("agent.status"));

        let payload = match parsed {
            JsonRpcMessage::Notification { params, .. } => params,
            _ => None,
        }
        .expect("params should be present");
        let parsed_params: AgentStatusParams = serde_json::from_value(payload)?;
        assert_eq!(parsed_params.state, "ready");
        assert_eq!(parsed_params.identity.as_deref(), Some("npub1test"));
        assert_eq!(
            parsed_params.capabilities,
            vec!["ReadMessages".to_string(), "SendMessages".to_string()]
        );

        let minimal = AgentStatusParams {
            state: "initializing".into(),
            identity: None,
            capabilities: vec![],
        };
        let minimal_json = serde_json::to_value(&minimal)?;
        assert!(minimal_json.get("state").is_some());
        assert!(minimal_json.get("identity").is_none());
        assert!(minimal_json.get("capabilities").is_none());

        Ok(())
    }

    #[test]
    fn invalid_json_returns_parse_error() {
        let err = parse_message("not valid json").unwrap_err();
        let rpc: JsonRpcError = err.into();
        assert_eq!(rpc.code, -32700);
    }

    #[test]
    fn missing_jsonrpc_field_is_rejected() {
        let err = parse_message(r#"{"id":1,"method":"agent.version"}"#).unwrap_err();
        let rpc: JsonRpcError = err.into();
        assert_eq!(rpc.code, -32600);
        assert!(rpc.message.to_lowercase().contains("jsonrpc"));
    }

    #[test]
    fn wrong_jsonrpc_value_is_rejected() {
        let err =
            parse_message(r#"{"jsonrpc":"1.0","id":1,"method":"agent.version"}"#).unwrap_err();
        let rpc: JsonRpcError = err.into();
        assert_eq!(rpc.code, -32600);
        assert!(rpc.message.to_lowercase().contains("jsonrpc"));
    }

    #[test]
    fn valid_jsonrpc_field_is_accepted() {
        let msg = parse_message(r#"{"jsonrpc":"2.0","id":1,"method":"agent.version"}"#).unwrap();
        assert_eq!(msg.id(), Some(&Value::from(1)));
        assert_eq!(msg.method(), Some("agent.version"));
    }
}

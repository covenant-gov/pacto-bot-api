# Generated from schemas/jsonrpc.json — do not edit manually.
# Run `cargo xtask codegen` to regenerate.

from __future__ import annotations
from typing import Any, ClassVar

from pydantic import BaseModel

"""Pydantic models generated from schemas/jsonrpc.json."""

# Result type alias for `agent.metrics`.
AgentMetricsResult = dict[str, Any]

# Result type alias for `agent.send_dm`.
AgentSendDmResult = str

# Result type alias for `agent.set_profile`.
AgentSetProfileResult = str

# Result type alias for `agent.version`.
AgentVersionResult = dict[str, Any]

class AdminSendTestDmParams(BaseModel):
    """
    Model for JSON-RPC method `admin.send_test_dm`.

    Send a test DM as the specified bot (admin-only).

    Example:

        >>> AdminSendTestDmParams(bot_id="...", content="...", recipient="...")

    jsonrpc_method: ``"admin.send_test_dm"``
    """
    jsonrpc_method: ClassVar[str] = "admin.send_test_dm"
    # Bot identity that will send the message.
    bot_id: str
    # Plaintext message body.
    content: str
    # Nostr public key (npub or hex) of the recipient.
    recipient: str


class AdminSendTestDmResult(BaseModel):
    """
    Model for JSON-RPC method `admin.send_test_dm`.

    Send a test DM as the specified bot (admin-only).

    Example:

        >>> AdminSendTestDmResult(event_id="...")

    jsonrpc_method: ``"admin.send_test_dm"``
    """
    jsonrpc_method: ClassVar[str] = "admin.send_test_dm"
    event_id: str


class AgentErrorParams(BaseModel):
    """
    Model for JSON-RPC method `agent.error`.

    Report an error encountered by a handler.

    Example:

        >>> AgentErrorParams(bot_id="...", message="...")

    jsonrpc_method: ``"agent.error"``
    """
    jsonrpc_method: ClassVar[str] = "agent.error"
    # Bot identity the error relates to.
    bot_id: str
    # Optional stable error code.
    code: str | None = None
    # Optional opaque structured context.
    data: Any | None = None
    # Human-readable, redacted error message.
    message: str


class AgentEventParams(BaseModel):
    """
    Model for JSON-RPC method `agent.event`.

    Notification of an incoming event for a registered bot.

    Example:

        >>> AgentEventParams(author="...", bot_id="...", content="...", event_id="...", rumor_id="...", timestamp=0, type="...")

    jsonrpc_method: ``"agent.event"``
    """
    jsonrpc_method: ClassVar[str] = "agent.event"
    # Public key of the original message author.
    author: str
    # Bot identity the event is for.
    bot_id: str
    # Conversation identifier, often the sender's npub.
    chat_id: str | None = None
    # Decrypted rumor content.
    content: str
    # Hex id of the enclosing gift-wrap event.
    event_id: str
    # Hex id of the decrypted rumor.
    rumor_id: str
    # Unix timestamp of the rumor.
    timestamp: int
    # Event type.
    type: str


class AgentListHandlersParams(BaseModel):
    """
    Model for JSON-RPC method `agent.list_handlers`.

    Return the daemon's handler routing table (admin-only).

    jsonrpc_method: ``"agent.list_handlers"``
    """
    jsonrpc_method: ClassVar[str] = "agent.list_handlers"
    pass

class AgentListHandlersResult(BaseModel):
    """
    Model for JSON-RPC method `agent.list_handlers`.

    Return the daemon's handler routing table (admin-only).

    Example:

        >>> AgentListHandlersResult(handlers=[])

    jsonrpc_method: ``"agent.list_handlers"``
    """
    jsonrpc_method: ClassVar[str] = "agent.list_handlers"
    handlers: list[AgentListHandlersResultHandlersModel]


class AgentListHandlersResultHandlersModel(BaseModel):
    """
    Model for JSON-RPC method `agent.list_handlers`.

    Nested object for `handlers` of `agent.list_handlers`.

    Example:

        >>> AgentListHandlersResultHandlersModel(bot_ids=[], capabilities=[], connected=True, event_types=[], handler_id="...", last_seen="...", registered_at="...", state="...", transport="...")

    jsonrpc_method: ``"agent.list_handlers"``
    """
    jsonrpc_method: ClassVar[str] = "agent.list_handlers"
    bot_ids: list[str]
    capabilities: list[str]
    connected: bool
    event_types: list[str]
    handler_id: str
    last_seen: str
    registered_at: str
    state: str
    transport: str


class AgentMetricsParams(BaseModel):
    """
    Model for JSON-RPC method `agent.metrics`.

    Return a machine-readable health and metrics snapshot.

    jsonrpc_method: ``"agent.metrics"``
    """
    jsonrpc_method: ClassVar[str] = "agent.metrics"
    pass

class AgentSendDmParams(BaseModel):
    """
    Model for JSON-RPC method `agent.send_dm`.

    Send a direct message as the specified bot.

    Example:

        >>> AgentSendDmParams(bot_id="...", content="...", recipient="...")

    jsonrpc_method: ``"agent.send_dm"``
    """
    jsonrpc_method: ClassVar[str] = "agent.send_dm"
    # Bot identity that will send the message.
    bot_id: str
    # Plaintext message body.
    content: str
    # Nostr public key (npub or hex) of the recipient.
    recipient: str
    # Optional hex event id this message replies to.
    reply_to: str | None = None


class AgentSetProfileParams(BaseModel):
    """
    Model for JSON-RPC method `agent.set_profile`.

    Update the bot's Nostr kind:0 profile.

    Example:

        >>> AgentSetProfileParams(bot_id="...")

    jsonrpc_method: ``"agent.set_profile"``
    """
    jsonrpc_method: ClassVar[str] = "agent.set_profile"
    # Free-form bio or description.
    about: str | None = None
    # Bot identity whose profile will be updated.
    bot_id: str
    # Display name.
    name: str | None = None
    # URL to a profile picture.
    picture: str | None = None


class AgentStatusParams(BaseModel):
    """
    Model for JSON-RPC method `agent.status`.

    Daemon lifecycle status notification.

    Example:

        >>> AgentStatusParams(state="...")

    jsonrpc_method: ``"agent.status"``
    """
    jsonrpc_method: ClassVar[str] = "agent.status"
    # Capabilities available to the handler.
    capabilities: list[str] | None = None
    # Public key of the bot whose state changed, when applicable.
    identity: str | None = None
    # Current daemon lifecycle state.
    state: str


class AgentUnregisterHandlerParams(BaseModel):
    """
    Model for JSON-RPC method `agent.unregister_handler`.

    Forcibly remove a handler from the routing table. The caller must be the target handler itself or have the Admin capability.

    Example:

        >>> AgentUnregisterHandlerParams(handler_id="...")

    jsonrpc_method: ``"agent.unregister_handler"``
    """
    jsonrpc_method: ClassVar[str] = "agent.unregister_handler"
    handler_id: str


class AgentUnregisterHandlerResult(BaseModel):
    """
    Model for JSON-RPC method `agent.unregister_handler`.

    Forcibly remove a handler from the routing table. The caller must be the target handler itself or have the Admin capability.

    Example:

        >>> AgentUnregisterHandlerResult(unregistered=True)

    jsonrpc_method: ``"agent.unregister_handler"``
    """
    jsonrpc_method: ClassVar[str] = "agent.unregister_handler"
    unregistered: bool


class AgentVersionParams(BaseModel):
    """
    Model for JSON-RPC method `agent.version`.

    Return the daemon version and git commit hash.

    jsonrpc_method: ``"agent.version"``
    """
    jsonrpc_method: ClassVar[str] = "agent.version"
    pass

class HandlerReconnectParams(BaseModel):
    """
    Model for JSON-RPC method `handler.reconnect`.

    Reconnect a previously registered handler using its secret reconnect token.

    Example:

        >>> HandlerReconnectParams(handler_id="...", reconnect_token="...")

    jsonrpc_method: ``"handler.reconnect"``
    """
    jsonrpc_method: ClassVar[str] = "handler.reconnect"
    # Server-generated handler id from the original registration.
    handler_id: str
    # Secret reconnect token returned by the original registration.
    reconnect_token: str


class HandlerReconnectResult(BaseModel):
    """
    Model for JSON-RPC method `handler.reconnect`.

    Reconnect a previously registered handler using its secret reconnect token.

    Example:

        >>> HandlerReconnectResult(handler_id="...", registered_events=[])

    jsonrpc_method: ``"handler.reconnect"``
    """
    jsonrpc_method: ClassVar[str] = "handler.reconnect"
    # Server-generated UUID for this handler.
    handler_id: str
    # Event types the handler is now subscribed to.
    registered_events: list[str]


class HandlerRegisterParams(BaseModel):
    """
    Model for JSON-RPC method `handler.register`.

    Register a handler connection for event delivery.

    Example:

        >>> HandlerRegisterParams(bot_ids=[], capabilities=[], event_types=[])

    jsonrpc_method: ``"handler.register"``
    """
    jsonrpc_method: ClassVar[str] = "handler.register"
    # Bot identities this handler wants to serve.
    bot_ids: list[str]
    # Capabilities the handler requests.
    capabilities: list[str]
    # Event types the handler wants to receive.
    event_types: list[str]


class HandlerRegisterResult(BaseModel):
    """
    Model for JSON-RPC method `handler.register`.

    Register a handler connection for event delivery.

    Example:

        >>> HandlerRegisterResult(handler_id="...", reconnect_token="...", registered_events=[])

    jsonrpc_method: ``"handler.register"``
    """
    jsonrpc_method: ClassVar[str] = "handler.register"
    # Server-generated UUID for this handler.
    handler_id: str
    # Server-generated secret token for reconnecting this handler.
    reconnect_token: str
    # Event types the handler is now subscribed to.
    registered_events: list[str]


class HandlerResponseParams(BaseModel):
    """
    Model for JSON-RPC method `handler.response`.

    Handler reply to a delivered agent.event.

    Example:

        >>> HandlerResponseParams(action="...", event_id="...")

    jsonrpc_method: ``"handler.response"``
    """
    jsonrpc_method: ClassVar[str] = "handler.response"
    # Terminal action the daemon should take.
    action: str
    # Reply text when action is 'reply'.
    content: str | None = None
    # Hex event id the handler is responding to.
    event_id: str


class HandlerUnregisterParams(BaseModel):
    """
    Model for JSON-RPC method `handler.unregister`.

    Remove a handler from the routing table.

    jsonrpc_method: ``"handler.unregister"``
    """
    jsonrpc_method: ClassVar[str] = "handler.unregister"
    pass

class HandlerUnregisterResult(BaseModel):
    """
    Model for JSON-RPC method `handler.unregister`.

    Remove a handler from the routing table.

    Example:

        >>> HandlerUnregisterResult(unregistered=True)

    jsonrpc_method: ``"handler.unregister"``
    """
    jsonrpc_method: ClassVar[str] = "handler.unregister"
    # True when the connection's handler was removed.
    unregistered: bool


__all__: list[str] = ['AgentMetricsResult', 'AgentSendDmResult', 'AgentSetProfileResult', 'AgentVersionResult', 'AdminSendTestDmParams', 'AdminSendTestDmResult', 'AgentErrorParams', 'AgentEventParams', 'AgentListHandlersParams', 'AgentListHandlersResult', 'AgentListHandlersResultHandlersModel', 'AgentMetricsParams', 'AgentSendDmParams', 'AgentSetProfileParams', 'AgentStatusParams', 'AgentUnregisterHandlerParams', 'AgentUnregisterHandlerResult', 'AgentVersionParams', 'HandlerReconnectParams', 'HandlerReconnectResult', 'HandlerRegisterParams', 'HandlerRegisterResult', 'HandlerResponseParams', 'HandlerUnregisterParams', 'HandlerUnregisterResult']

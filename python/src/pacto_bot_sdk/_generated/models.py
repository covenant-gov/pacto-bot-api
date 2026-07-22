# Generated from schemas/jsonrpc.json — do not edit manually.
# Run `cargo xtask codegen` to regenerate.

from __future__ import annotations
from typing import Any, ClassVar

from pydantic import BaseModel

"""Pydantic models generated from schemas/jsonrpc.json."""

# Result type alias for `agent.metrics`.
AgentMetricsResponse = dict[str, Any]

# Result type alias for `agent.publish_key_package`.
AgentPublishKeyPackageResponse = str

# Result type alias for `agent.send_dm`.
AgentSendDmResponse = str

# Result type alias for `agent.send_group_message`.
AgentSendGroupMessageResponse = str

# Result type alias for `agent.set_profile`.
AgentSetProfileResponse = str

# Result type alias for `agent.version`.
AgentVersionResponse = dict[str, Any]

# Result type alias for `system.health`.
SystemHealthResponse = dict[str, Any]

# Result type alias for `system.version`.
SystemVersionResponse = dict[str, Any]

class AdminCreateMlsGroupParams(BaseModel):
    """
    Model for JSON-RPC method `admin.create_mls_group`.

    Create a new MLS group and invite the recipient (admin-only).

    Example:

        >>> AdminCreateMlsGroupParams(bot_id="...", group_name="...", recipient="...")

    jsonrpc_method: ``"admin.create_mls_group"``
    """
    jsonrpc_method: ClassVar[str] = "admin.create_mls_group"
    # Bot identity that will own the group.
    bot_id: str
    # Human-readable group name.
    group_name: str
    # Nostr public key (npub or hex) of the initial member.
    recipient: str


class AdminCreateMlsGroupResponse(BaseModel):
    """
    Model for JSON-RPC method `admin.create_mls_group`.

    Create a new MLS group and invite the recipient (admin-only).

    Example:

        >>> AdminCreateMlsGroupResponse(wire_id="...")

    jsonrpc_method: ``"admin.create_mls_group"``
    """
    jsonrpc_method: ClassVar[str] = "admin.create_mls_group"
    wire_id: str


class AdminInviteToMlsGroupParams(BaseModel):
    """
    Model for JSON-RPC method `admin.invite_to_mls_group`.

    Invite a recipient to an existing MLS group (admin-only).

    Example:

        >>> AdminInviteToMlsGroupParams(bot_id="...", group_name="...", recipient="...")

    jsonrpc_method: ``"admin.invite_to_mls_group"``
    """
    jsonrpc_method: ClassVar[str] = "admin.invite_to_mls_group"
    # Bot identity that owns the group.
    bot_id: str
    # Human-readable group name.
    group_name: str
    # Nostr public key (npub or hex) of the member to invite.
    recipient: str


class AdminInviteToMlsGroupResponse(BaseModel):
    """
    Model for JSON-RPC method `admin.invite_to_mls_group`.

    Invite a recipient to an existing MLS group (admin-only).

    Example:

        >>> AdminInviteToMlsGroupResponse(wire_id="...")

    jsonrpc_method: ``"admin.invite_to_mls_group"``
    """
    jsonrpc_method: ClassVar[str] = "admin.invite_to_mls_group"
    wire_id: str


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


class AdminSendTestDmResponse(BaseModel):
    """
    Model for JSON-RPC method `admin.send_test_dm`.

    Send a test DM as the specified bot (admin-only).

    Example:

        >>> AdminSendTestDmResponse(event_id="...")

    jsonrpc_method: ``"admin.send_test_dm"``
    """
    jsonrpc_method: ClassVar[str] = "admin.send_test_dm"
    event_id: str


class AgentCreateMlsGroupParams(BaseModel):
    """
    Model for JSON-RPC method `agent.create_mls_group`.

    Create a new MLS group and invite the recipient.

    Example:

        >>> AgentCreateMlsGroupParams(bot_id="...", group_name="...", recipient="...")

    jsonrpc_method: ``"agent.create_mls_group"``
    """
    jsonrpc_method: ClassVar[str] = "agent.create_mls_group"
    # Bot identity that will own the group.
    bot_id: str
    # Human-readable group name.
    group_name: str
    # Nostr public key (npub or hex) of the initial member.
    recipient: str


class AgentCreateMlsGroupResponse(BaseModel):
    """
    Model for JSON-RPC method `agent.create_mls_group`.

    Create a new MLS group and invite the recipient.

    Example:

        >>> AgentCreateMlsGroupResponse(wire_id="...")

    jsonrpc_method: ``"agent.create_mls_group"``
    """
    jsonrpc_method: ClassVar[str] = "agent.create_mls_group"
    wire_id: str


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

    Notification of an incoming event for a registered bot. For squad messages, the daemon forwards the parsed mention envelope and computed mention metadata (is_mentioned, mentioned_bot_ids, mentions) alongside the message content.

    Example:

        >>> AgentEventParams(author="...", bot_id="...", content="...", event_id="...", rumor_id="...", timestamp=0, type="...")

    jsonrpc_method: ``"agent.event"``
    """
    jsonrpc_method: ClassVar[str] = "agent.event"
    # Public key of the original message author.
    author: str
    # Bot identity the event is for.
    bot_id: str
    # Conversation identifier; the sender's npub for DMs or the Squad wire id for MLS welcome and group messages.
    chat_id: str | None = None
    # Decrypted rumor content.
    content: str
    # Hex id of the enclosing gift-wrap event.
    event_id: str
    # Whether the receiving bot's npub appears in `mentions`.
    is_mentioned: bool = False
    # Configured `bot_id` values whose npubs appear in `mentions`.
    mentioned_bot_ids: list[str] = []
    # Target npubs from the mention envelope; empty for DMs and legacy squad messages.
    mentions: list[str] = []
    # Virtual bucket identifier from the mention envelope; omitted for DMs and legacy squad messages.
    pacto_virtual_bucket: str | None = None
    # Hex id of the decrypted rumor.
    rumor_id: str
    # Unix timestamp of the rumor.
    timestamp: int
    # Event type.
    type: str


class AgentExitMlsGroupParams(BaseModel):
    """
    Model for JSON-RPC method `agent.exit_mls_group`.

    Exit a Squad by publishing a self-removal MLS proposal.

    Example:

        >>> AgentExitMlsGroupParams(bot_id="...", group_id="...")

    jsonrpc_method: ``"agent.exit_mls_group"``
    """
    jsonrpc_method: ClassVar[str] = "agent.exit_mls_group"
    # Bot identity that participates in the Squad.
    bot_id: str
    # Hex-encoded Squad wire id.
    group_id: str


class AgentExitMlsGroupResponse(BaseModel):
    """
    Model for JSON-RPC method `agent.exit_mls_group`.

    Exit a Squad by publishing a self-removal MLS proposal.

    Example:

        >>> AgentExitMlsGroupResponse(event_id="...")

    jsonrpc_method: ``"agent.exit_mls_group"``
    """
    jsonrpc_method: ClassVar[str] = "agent.exit_mls_group"
    # Hex id of the published kind:445 evolution event containing the leave proposal.
    event_id: str


class AgentInviteToMlsGroupParams(BaseModel):
    """
    Model for JSON-RPC method `agent.invite_to_mls_group`.

    Invite a recipient to an existing MLS group.

    Example:

        >>> AgentInviteToMlsGroupParams(bot_id="...", group_name="...", recipient="...")

    jsonrpc_method: ``"agent.invite_to_mls_group"``
    """
    jsonrpc_method: ClassVar[str] = "agent.invite_to_mls_group"
    # Bot identity that owns the group.
    bot_id: str
    # Human-readable group name.
    group_name: str
    # Nostr public key (npub or hex) of the member to invite.
    recipient: str


class AgentInviteToMlsGroupResponse(BaseModel):
    """
    Model for JSON-RPC method `agent.invite_to_mls_group`.

    Invite a recipient to an existing MLS group.

    Example:

        >>> AgentInviteToMlsGroupResponse(wire_id="...")

    jsonrpc_method: ``"agent.invite_to_mls_group"``
    """
    jsonrpc_method: ClassVar[str] = "agent.invite_to_mls_group"
    wire_id: str


class AgentIsSquadMemberParams(BaseModel):
    """
    Model for JSON-RPC method `agent.is_squad_member`.

    Verify whether a Nostr public key is a member of a Squad.

    Example:

        >>> AgentIsSquadMemberParams(bot_id="...", group_id="...", member_pubkey="...")

    jsonrpc_method: ``"agent.is_squad_member"``
    """
    jsonrpc_method: ClassVar[str] = "agent.is_squad_member"
    # Bot identity that participates in the Squad.
    bot_id: str
    # Hex-encoded Squad wire id.
    group_id: str
    # Nostr public key (npub or hex) of the member to check.
    member_pubkey: str


class AgentIsSquadMemberResponse(BaseModel):
    """
    Model for JSON-RPC method `agent.is_squad_member`.

    Verify whether a Nostr public key is a member of a Squad.

    Example:

        >>> AgentIsSquadMemberResponse(is_member=True)

    jsonrpc_method: ``"agent.is_squad_member"``
    """
    jsonrpc_method: ClassVar[str] = "agent.is_squad_member"
    # True when the public key is a member of the Squad.
    is_member: bool


class AgentListHandlersParams(BaseModel):
    """
    Model for JSON-RPC method `agent.list_handlers`.

    Return the daemon's handler routing table (admin-only).

    jsonrpc_method: ``"agent.list_handlers"``
    """
    jsonrpc_method: ClassVar[str] = "agent.list_handlers"
    pass

class AgentListHandlersResponse(BaseModel):
    """
    Model for JSON-RPC method `agent.list_handlers`.

    Return the daemon's handler routing table (admin-only).

    Example:

        >>> AgentListHandlersResponse(handlers=[])

    jsonrpc_method: ``"agent.list_handlers"``
    """
    jsonrpc_method: ClassVar[str] = "agent.list_handlers"
    handlers: list[AgentListHandlersResponseHandlersModel]


class AgentListHandlersResponseHandlersModel(BaseModel):
    """
    Model for JSON-RPC method `agent.list_handlers`.

    Nested object for `handlers` of `agent.list_handlers`.

    Example:

        >>> AgentListHandlersResponseHandlersModel(bot_ids=[], capabilities=[], connected=True, event_types=[], handler_id="...", last_seen="...", registered_at="...", state="...", transport="...")

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

class AgentPublishKeyPackageParams(BaseModel):
    """
    Model for JSON-RPC method `agent.publish_key_package`.

    Publish a Nostr MLS KeyPackage event (kind:443) for the specified bot.

    Example:

        >>> AgentPublishKeyPackageParams(bot_id="...")

    jsonrpc_method: ``"agent.publish_key_package"``
    """
    jsonrpc_method: ClassVar[str] = "agent.publish_key_package"
    # Bot identity that will publish the KeyPackage.
    bot_id: str


class AgentRateLimitedParams(BaseModel):
    """
    Model for JSON-RPC method `agent.rate_limited`.

    Notification that an inbound MLS group message was dropped because the per-Squad rate limit was exceeded.

    Example:

        >>> AgentRateLimitedParams(bot_id="...", group_id="...", window_seconds=0)

    jsonrpc_method: ``"agent.rate_limited"``
    """
    jsonrpc_method: ClassVar[str] = "agent.rate_limited"
    # Bot identity the Squad belongs to.
    bot_id: str
    # Hex-encoded Squad wire id.
    group_id: str
    # Duration of the rate-limit window in seconds.
    window_seconds: int


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


class AgentSendGroupMessageParams(BaseModel):
    """
    Model for JSON-RPC method `agent.send_group_message`.

    Send an encrypted MLS group message as the specified bot.

    Example:

        >>> AgentSendGroupMessageParams(bot_id="...", content="...", group_id="...")

    jsonrpc_method: ``"agent.send_group_message"``
    """
    jsonrpc_method: ClassVar[str] = "agent.send_group_message"
    # Bot identity that will send the message.
    bot_id: str
    # Plaintext message body to encrypt.
    content: str
    # Hex-encoded MLS group ID.
    group_id: str
    # Optional virtual bucket identifier; when provided, the content is wrapped in the pacto mention envelope before MLS encryption.
    pacto_virtual_bucket: str | None = None


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


class AgentUnregisterHandlerResponse(BaseModel):
    """
    Model for JSON-RPC method `agent.unregister_handler`.

    Forcibly remove a handler from the routing table. The caller must be the target handler itself or have the Admin capability.

    Example:

        >>> AgentUnregisterHandlerResponse(unregistered=True)

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


class HandlerReconnectResponse(BaseModel):
    """
    Model for JSON-RPC method `handler.reconnect`.

    Reconnect a previously registered handler using its secret reconnect token.

    Example:

        >>> HandlerReconnectResponse(handler_id="...", registered_events=[])

    jsonrpc_method: ``"handler.reconnect"``
    """
    jsonrpc_method: ClassVar[str] = "handler.reconnect"
    # Server-generated UUID for this handler.
    handler_id: str
    # Map from daemon-local bot_id to the bot's Nostr public key (npub).
    own_pubkeys: dict[str, str] = None
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
    # Capabilities the handler requests. Valid values include ReadMessages, SendMessages, ManageProfile, SendGroupMessages, ReceiveGroupMessages, CreateMlsGroup, InviteToMlsGroup, and ExitMlsGroup.
    capabilities: list[str]
    # Event types the handler wants to receive.
    event_types: list[str]


class HandlerRegisterResponse(BaseModel):
    """
    Model for JSON-RPC method `handler.register`.

    Register a handler connection for event delivery.

    Example:

        >>> HandlerRegisterResponse(handler_id="...", reconnect_token="...", registered_events=[])

    jsonrpc_method: ``"handler.register"``
    """
    jsonrpc_method: ClassVar[str] = "handler.register"
    # Server-generated UUID for this handler.
    handler_id: str
    # Map from daemon-local bot_id to the bot's Nostr public key (npub).
    own_pubkeys: dict[str, str] = None
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

class HandlerUnregisterResponse(BaseModel):
    """
    Model for JSON-RPC method `handler.unregister`.

    Remove a handler from the routing table.

    Example:

        >>> HandlerUnregisterResponse(unregistered=True)

    jsonrpc_method: ``"handler.unregister"``
    """
    jsonrpc_method: ClassVar[str] = "handler.unregister"
    # True when the connection's handler was removed.
    unregistered: bool


class SystemHealthParams(BaseModel):
    """
    Model for JSON-RPC method `system.health`.

    Return a machine-readable health and metrics snapshot.

    jsonrpc_method: ``"system.health"``
    """
    jsonrpc_method: ClassVar[str] = "system.health"
    pass

class SystemVersionParams(BaseModel):
    """
    Model for JSON-RPC method `system.version`.

    Return the daemon version and git commit hash.

    jsonrpc_method: ``"system.version"``
    """
    jsonrpc_method: ClassVar[str] = "system.version"
    pass

__all__: list[str] = ['AgentMetricsResponse', 'AgentPublishKeyPackageResponse', 'AgentSendDmResponse', 'AgentSendGroupMessageResponse', 'AgentSetProfileResponse', 'AgentVersionResponse', 'SystemHealthResponse', 'SystemVersionResponse', 'AdminCreateMlsGroupParams', 'AdminCreateMlsGroupResponse', 'AdminInviteToMlsGroupParams', 'AdminInviteToMlsGroupResponse', 'AdminSendTestDmParams', 'AdminSendTestDmResponse', 'AgentCreateMlsGroupParams', 'AgentCreateMlsGroupResponse', 'AgentErrorParams', 'AgentEventParams', 'AgentExitMlsGroupParams', 'AgentExitMlsGroupResponse', 'AgentInviteToMlsGroupParams', 'AgentInviteToMlsGroupResponse', 'AgentIsSquadMemberParams', 'AgentIsSquadMemberResponse', 'AgentListHandlersParams', 'AgentListHandlersResponse', 'AgentListHandlersResponseHandlersModel', 'AgentMetricsParams', 'AgentPublishKeyPackageParams', 'AgentRateLimitedParams', 'AgentSendDmParams', 'AgentSendGroupMessageParams', 'AgentSetProfileParams', 'AgentStatusParams', 'AgentUnregisterHandlerParams', 'AgentUnregisterHandlerResponse', 'AgentVersionParams', 'HandlerReconnectParams', 'HandlerReconnectResponse', 'HandlerRegisterParams', 'HandlerRegisterResponse', 'HandlerResponseParams', 'HandlerUnregisterParams', 'HandlerUnregisterResponse', 'SystemHealthParams', 'SystemVersionParams']

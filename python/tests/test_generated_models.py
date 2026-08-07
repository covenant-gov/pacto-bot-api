"""Tests for the Pydantic models generated from schemas/jsonrpc.json."""

from __future__ import annotations

from typing import Any

import pytest
from pydantic import ValidationError

from pacto_bot_sdk import (
    AgentErrorParams,
    AgentEventParams,
    AgentMetricsParams,
    AgentMetricsResponse,
    AgentSendAttachmentParams,
    AgentSendAttachmentResponse,
    AgentSendDmParams,
    AgentSendDmResponse,
    AgentSendGroupAttachmentParams,
    AgentSendGroupAttachmentResponse,
    AgentSendGroupReactionParams,
    AgentSendGroupReactionResponse,
    AgentSendReactionParams,
    AgentSendReactionResponse,
    AgentSetProfileParams,
    AgentSetProfileResponse,
    AgentStatusParams,
    AgentVersionParams,
    AgentVersionResponse,
    HandlerRegisterParams,
    HandlerRegisterResponse,
    HandlerResponseParams,
    HandlerUnregisterParams,
    HandlerUnregisterResponse,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _assert_required_fields(
    model_cls: type[Any],
    sample: dict[str, Any],
    required_fields: list[str],
) -> None:
    """Verify that omitting each required field raises ``ValidationError``."""
    for field in required_fields:
        missing = {k: v for k, v in sample.items() if k != field}
        with pytest.raises(ValidationError) as exc_info:
            model_cls(**missing)
        assert field in str(exc_info.value)


def _assert_optional_defaults_to_none(
    model_cls: type[Any],
    sample: dict[str, Any],
    optional_fields: list[str],
) -> None:
    """Verify that optional fields default to ``None`` when omitted."""
    instance = model_cls(**sample)
    for field in optional_fields:
        assert getattr(instance, field) is None


# ---------------------------------------------------------------------------
# Result type aliases
# ---------------------------------------------------------------------------


def test_result_type_aliases_are_expected_types():
    """The unresolved-schema result aliases keep their declared shapes."""
    assert AgentMetricsResponse == dict[str, Any]
    assert AgentVersionResponse == dict[str, Any]
    assert AgentSendDmResponse == str
    assert AgentSetProfileResponse == str


# ---------------------------------------------------------------------------
# AgentEventParams
# ---------------------------------------------------------------------------


def test_agent_event_params_constructs_with_valid_data():
    params = AgentEventParams(
        author="npub1author",
        bot_id="test-bot",
        chat_id="npub1chat",
        content="/hello",
        event_id="e-1",
        rumor_id="r-1",
        timestamp=1234567890,
        type="dm_received",
    )
    assert params.bot_id == "test-bot"
    assert params.content == "/hello"
    assert params.chat_id == "npub1chat"
    assert params.jsonrpc_method == "agent.event"


def test_agent_event_params_optional_chat_id_defaults_to_none():
    params = AgentEventParams(
        author="npub1author",
        bot_id="test-bot",
        content="/hello",
        event_id="e-1",
        rumor_id="r-1",
        timestamp=1234567890,
        type="dm_received",
    )
    assert params.chat_id is None


def test_agent_event_params_requires_all_mandatory_fields():
    sample = {
        "author": "npub1author",
        "bot_id": "test-bot",
        "content": "/hello",
        "event_id": "e-1",
        "rumor_id": "r-1",
        "timestamp": 1234567890,
        "type": "dm_received",
    }
    _assert_required_fields(
        AgentEventParams, sample, ["author", "bot_id", "content", "event_id", "rumor_id", "timestamp", "type"]
    )


# ---------------------------------------------------------------------------
# AgentSendDmParams
# ---------------------------------------------------------------------------


def test_agent_send_dm_params_constructs_with_valid_data():
    params = AgentSendDmParams(
        bot_id="test-bot",
        content="Hello!",
        recipient="npub1recipient",
        reply_to="e-reply",
    )
    assert params.bot_id == "test-bot"
    assert params.recipient == "npub1recipient"
    assert params.reply_to == "e-reply"
    assert params.jsonrpc_method == "agent.send_dm"


def test_agent_send_dm_params_optional_reply_to_defaults_to_none():
    sample = {
        "bot_id": "test-bot",
        "content": "Hello!",
        "recipient": "npub1recipient",
    }
    _assert_optional_defaults_to_none(AgentSendDmParams, sample, ["reply_to"])


def test_agent_send_dm_params_requires_mandatory_fields():
    sample = {
        "bot_id": "test-bot",
        "content": "Hello!",
        "recipient": "npub1recipient",
    }
    _assert_required_fields(AgentSendDmParams, sample, ["bot_id", "content", "recipient"])


# ---------------------------------------------------------------------------
# AgentSetProfileParams
# ---------------------------------------------------------------------------


def test_agent_set_profile_params_constructs_with_valid_data():
    params = AgentSetProfileParams(
        bot_id="test-bot",
        name="Test Bot",
        about="A friendly bot",
        picture="https://example.com/bot.png",
    )
    assert params.bot_id == "test-bot"
    assert params.name == "Test Bot"
    assert params.about == "A friendly bot"
    assert params.picture == "https://example.com/bot.png"
    assert params.jsonrpc_method == "agent.set_profile"


def test_agent_set_profile_params_optional_fields_default_to_none():
    params = AgentSetProfileParams(bot_id="test-bot")
    assert params.about is None
    assert params.name is None
    assert params.picture is None


def test_agent_set_profile_params_requires_bot_id():
    with pytest.raises(ValidationError):
        AgentSetProfileParams(name="Test Bot")


# ---------------------------------------------------------------------------
# AgentErrorParams
# ---------------------------------------------------------------------------


def test_agent_error_params_constructs_with_valid_data():
    params = AgentErrorParams(
        bot_id="test-bot",
        message="Something went wrong",
        code="E123",
        data={"detail": "extra"},
    )
    assert params.bot_id == "test-bot"
    assert params.message == "Something went wrong"
    assert params.code == "E123"
    assert params.data == {"detail": "extra"}
    assert params.jsonrpc_method == "agent.error"


def test_agent_error_params_optional_fields_default_to_none():
    params = AgentErrorParams(bot_id="test-bot", message="Oops")
    assert params.code is None
    assert params.data is None


def test_agent_error_params_requires_bot_id_and_message():
    sample = {"bot_id": "test-bot", "message": "Oops"}
    _assert_required_fields(AgentErrorParams, sample, ["bot_id", "message"])


# ---------------------------------------------------------------------------
# AgentStatusParams
# ---------------------------------------------------------------------------


def test_agent_status_params_constructs_with_valid_data():
    params = AgentStatusParams(
        state="ready",
        daemon_version="0.10.0",
        mls_wire_generation="mip-00-02-base64",
        identity="npub1bot",
        capabilities=["ReadMessages", "SendMessages"],
    )
    assert params.state == "ready"
    assert params.daemon_version == "0.10.0"
    assert params.mls_wire_generation == "mip-00-02-base64"
    assert params.identity == "npub1bot"
    assert params.capabilities == ["ReadMessages", "SendMessages"]
    assert params.jsonrpc_method == "agent.status"


def test_agent_status_params_optional_fields_default_to_none():
    params = AgentStatusParams(
        state="ready", daemon_version="0.10.0", mls_wire_generation="mip-00-02-base64"
    )
    assert params.identity is None
    assert params.capabilities is None


def test_agent_status_params_requires_state():
    with pytest.raises(ValidationError):
        AgentStatusParams(identity="npub1bot")


# ---------------------------------------------------------------------------
# HandlerRegisterParams
# ---------------------------------------------------------------------------


def test_handler_register_params_constructs_with_valid_data():
    params = HandlerRegisterParams(
        bot_ids=["test-bot"],
        capabilities=["ReadMessages"],
        event_types=["dm_received"],
    )
    assert params.bot_ids == ["test-bot"]
    assert params.capabilities == ["ReadMessages"]
    assert params.event_types == ["dm_received"]
    assert params.jsonrpc_method == "handler.register"


def test_handler_register_params_requires_all_fields():
    sample = {
        "bot_ids": ["test-bot"],
        "capabilities": ["ReadMessages"],
        "event_types": ["dm_received"],
    }
    _assert_required_fields(
        HandlerRegisterParams, sample, ["bot_ids", "capabilities", "event_types"]
    )


# ---------------------------------------------------------------------------
    # HandlerRegisterResponse
# ---------------------------------------------------------------------------


def test_handler_register_result_constructs_with_valid_data():
    result = HandlerRegisterResponse(
        handler_id="h-123",
        reconnect_token="rt-123",
        registered_events=["dm_received"],
        spool_dir="/var/lib/pacto/spool/outbound",
    )
    assert result.handler_id == "h-123"
    assert result.registered_events == ["dm_received"]
    assert result.spool_dir == "/var/lib/pacto/spool/outbound"
    assert result.jsonrpc_method == "handler.register"


def test_handler_register_result_requires_all_fields():
    sample = {
        "handler_id": "h-123",
        "reconnect_token": "rt-123",
        "registered_events": ["dm_received"],
        "spool_dir": "/var/lib/pacto/spool/outbound",
    }
    _assert_required_fields(
        HandlerRegisterResponse,
        sample,
        ["handler_id", "reconnect_token", "registered_events", "spool_dir"],
    )


# ---------------------------------------------------------------------------
# HandlerResponseParams
# ---------------------------------------------------------------------------


def test_handler_response_params_constructs_with_valid_data():
    params = HandlerResponseParams(
        action="reply",
        event_id="e-1",
        content="Hi there!",
    )
    assert params.action == "reply"
    assert params.event_id == "e-1"
    assert params.content == "Hi there!"
    assert params.jsonrpc_method == "handler.response"


def test_handler_response_params_optional_content_defaults_to_none():
    params = HandlerResponseParams(action="ignore", event_id="e-1")
    assert params.content is None


def test_handler_response_params_requires_action_and_event_id():
    sample = {"action": "reply", "event_id": "e-1"}
    _assert_required_fields(HandlerResponseParams, sample, ["action", "event_id"])


# ---------------------------------------------------------------------------
    # HandlerUnregisterResponse
# ---------------------------------------------------------------------------


def test_handler_unregister_result_constructs_with_valid_data():
    result = HandlerUnregisterResponse(unregistered=True)
    assert result.unregistered is True
    assert result.jsonrpc_method == "handler.unregister"


def test_handler_unregister_result_requires_unregistered():
    with pytest.raises(ValidationError):
        HandlerUnregisterResponse()


# ---------------------------------------------------------------------------
# Empty parameter models
# ---------------------------------------------------------------------------


def test_agent_metrics_params_constructs():
    params = AgentMetricsParams()
    assert isinstance(params, AgentMetricsParams)


def test_agent_version_params_constructs():
    params = AgentVersionParams()
    assert isinstance(params, AgentVersionParams)


def test_handler_unregister_params_constructs():
    params = HandlerUnregisterParams()
    assert isinstance(params, HandlerUnregisterParams)

# ---------------------------------------------------------------------------
# Reaction and attachment wire surface
# ---------------------------------------------------------------------------


def test_new_event_type_wire_names_round_trip():
    for event_type in [
        "reaction_received",
        "attachment_received",
        "mls_group_reaction_received",
        "mls_group_attachment_received",
    ]:
        params = AgentEventParams(
            bot_id="test-bot",
            event_id="e-1",
            type=event_type,
            content="",
            rumor_id="r-1",
            author="npub1author",
            timestamp=1,
        )
        assert AgentEventParams.model_validate(params.model_dump()).type == event_type


def test_reaction_params_require_complete_targets():
    dm = {
        "bot_id": "test-bot",
        "recipient": "npub1recipient",
        "target_rumor_id": "ab" * 32,
        "emoji": "👍",
    }
    group = {
        "bot_id": "test-bot",
        "group_id": "cd" * 32,
        "target_rumor_id": "ab" * 32,
        "emoji": "👍",
    }
    _assert_required_fields(AgentSendReactionParams, dm, list(dm))
    _assert_required_fields(AgentSendGroupReactionParams, group, list(group))
    assert AgentSendReactionResponse is str
    assert AgentSendGroupReactionResponse is str


def test_attachment_params_require_target_and_default_metadata():
    dm = {"bot_id": "test-bot", "recipient": "npub1recipient"}
    group = {"bot_id": "test-bot", "group_id": "cd" * 32}
    optional = [
        "blurhash",
        "dim",
        "filename",
        "inline_base64",
        "reply_to",
        "spool_path",
    ]
    _assert_required_fields(AgentSendAttachmentParams, dm, list(dm))
    _assert_required_fields(AgentSendGroupAttachmentParams, group, list(group))
    _assert_optional_defaults_to_none(AgentSendAttachmentParams, dm, optional)
    _assert_optional_defaults_to_none(AgentSendGroupAttachmentParams, group, optional)
    assert AgentSendAttachmentResponse is str
    assert AgentSendGroupAttachmentResponse is str


def test_generated_models_all_exports_new_surface():
    from pacto_bot_sdk._generated import models

    expected = {
        "AgentEventParamsAttachmentModel",
        "AgentEventParamsReactionModel",
        "AgentSendAttachmentParams",
        "AgentSendAttachmentResponse",
        "AgentSendGroupAttachmentParams",
        "AgentSendGroupAttachmentResponse",
        "AgentSendGroupReactionParams",
        "AgentSendGroupReactionResponse",
        "AgentSendReactionParams",
        "AgentSendReactionResponse",
    }
    assert expected <= set(models.__all__)

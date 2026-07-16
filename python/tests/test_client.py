"""Tests for the generated low-level async client."""

from __future__ import annotations

import asyncio

import pytest

from pacto_bot_sdk import (
    AgentEventParams,
    AgentRateLimitedParams,
    HandlerRegisterParams,
    HandlerRegisterResponse,
    PactoClient,
    PactoClientError,
)
from conftest import MockTransport


@pytest.fixture
def transport(mock_transport):
    """Expose the shared mock transport under the local name."""
    return mock_transport


@pytest.mark.asyncio
async def test_handler_register_sends_frame_and_awaits_result(client, transport):
    """handler_register builds the right JSON-RPC frame and returns a parsed result."""
    task = asyncio.create_task(
        client.handler_register(
            bot_ids=["greeting-bot"],
            capabilities=["ReadMessages", "SendMessages"],
            event_types=["dm_received"],
        )
    )

    await asyncio.sleep(0)  # let the coroutine write the request
    assert len(transport.frames) == 1
    request = transport.frames[0]
    assert request["jsonrpc"] == "2.0"
    assert request["method"] == "handler.register"
    assert isinstance(request["id"], str)
    assert request["params"] == {
        "bot_ids": ["greeting-bot"],
        "capabilities": ["ReadMessages", "SendMessages"],
        "event_types": ["dm_received"],
    }

    transport.inject(
        {
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "handler_id": "h-123",
                "reconnect_token": "rt-123",
                "registered_events": ["dm_received"],
            },
        }
    )

    result = await task
    assert isinstance(result, HandlerRegisterResponse)
    assert result.handler_id == "h-123"
    assert result.registered_events == ["dm_received"]


@pytest.mark.asyncio
async def test_agent_error_notification_fire_and_forget(client, transport):
    """agent_error sends a notification frame with no id and returns immediately."""
    await client.agent_error(bot_id="greeting-bot", message="something went wrong")

    assert len(transport.frames) == 1
    frame = transport.frames[0]
    assert "id" not in frame
    assert frame["jsonrpc"] == "2.0"
    assert frame["method"] == "agent.error"
    assert frame["params"] == {
        "bot_id": "greeting-bot",
        "message": "something went wrong",
    }


@pytest.mark.asyncio
async def test_incoming_notification_async_iterator(client, transport):
    """Incoming agent.event frames are exposed by the notifications() iterator."""
    transport.inject(
        {
            "jsonrpc": "2.0",
            "method": "agent.event",
            "params": {
                "bot_id": "greeting-bot",
                "event_id": "e-1",
                "type": "dm_received",
                "chat_id": "npub1chat",
                "content": "/hello",
                "rumor_id": "r-1",
                "author": "npub1author",
                "timestamp": 1234567890,
            },
        }
    )

    notification = await anext(client.notifications())
    assert isinstance(notification, AgentEventParams)
    assert notification.bot_id == "greeting-bot"
    assert notification.content == "/hello"
    assert notification.chat_id == "npub1chat"


@pytest.mark.asyncio
async def test_incoming_rate_limited_notification_async_iterator(client, transport):
    """Incoming agent.rate_limited frames are exposed by the notifications() iterator."""
    transport.inject(
        {
            "jsonrpc": "2.0",
            "method": "agent.rate_limited",
            "params": {
                "bot_id": "greeting-bot",
                "group_id": "0xdeadbeef",
                "window_seconds": 30,
            },
        }
    )

    notification = await anext(client.notifications())
    assert isinstance(notification, AgentRateLimitedParams)
    assert notification.bot_id == "greeting-bot"
    assert notification.group_id == "0xdeadbeef"
    assert notification.window_seconds == 30


@pytest.mark.asyncio
async def test_mismatched_response_id_ignored(client, transport):
    """A response whose id is not in-flight is ignored without crashing."""
    transport.inject(
        {
            "jsonrpc": "2.0",
            "id": "unknown-id",
            "result": {"handler_id": "h-999", "registered_events": []},
        }
    )
    await asyncio.sleep(0)
    # No exception and no pending futures.
    assert not client._inflight


@pytest.mark.asyncio
async def test_handler_register_params_model_construction():
    """The generated params model validates and serializes correctly."""
    params = HandlerRegisterParams(
        bot_ids=["b1"],
        event_types=["dm_received"],
        capabilities=["ReadMessages"],
    )
    assert params.bot_ids == ["b1"]
    assert params.model_dump(mode="json", exclude_none=True) == {
        "bot_ids": ["b1"],
        "event_types": ["dm_received"],
        "capabilities": ["ReadMessages"],
    }
    assert params.jsonrpc_method == "handler.register"


@pytest.mark.asyncio
async def test_handler_register_result_model_construction():
    """The generated result model validates and exposes its fields."""
    result = HandlerRegisterResponse(
        handler_id="h-1", reconnect_token="rt-1", registered_events=["dm_received"]
    )
    assert result.handler_id == "h-1"
    assert result.jsonrpc_method == "handler.register"


@pytest.mark.asyncio
async def test_error_response_raises_pacto_client_error(client, transport):
    """A JSON-RPC error response is converted into ``PactoClientError``."""
    task = asyncio.create_task(
        client.handler_register(
            bot_ids=["greeting-bot"],
            capabilities=["ReadMessages", "SendMessages"],
            event_types=["dm_received"],
        )
    )

    await asyncio.sleep(0)
    request = transport.frames[0]
    transport.inject(
        {
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {"code": -32600, "message": "Invalid Request"},
        }
    )

    with pytest.raises(PactoClientError, match="Invalid Request") as exc_info:
        await task

    assert exc_info.value.code == -32600


@pytest.mark.asyncio
async def test_unmatched_request_times_out_when_no_response(client):
    """A request with no matching response waits until cancelled externally."""
    with pytest.raises(asyncio.TimeoutError):
        await asyncio.wait_for(
            client.handler_register(
                bot_ids=["greeting-bot"],
                capabilities=["ReadMessages", "SendMessages"],
                event_types=["dm_received"],
            ),
            timeout=0.05,
        )


def test_constructor_default_timeout():
    """PactoClient stores a default timeout of 30.0 when none is provided."""
    transport = MockTransport()
    client = PactoClient(transport)
    assert client._default_timeout == 30.0


def test_constructor_custom_timeout():
    """PactoClient stores the requested default timeout."""
    transport = MockTransport()
    client = PactoClient(transport, timeout=5.0)
    assert client._default_timeout == 5.0


@pytest.mark.asyncio
async def test_per_call_timeout_overrides_default(mock_transport):
    """A request method can override the client's default timeout."""
    client = PactoClient(mock_transport, timeout=0.01)
    await client.connect()
    try:
        task = asyncio.create_task(
            client.handler_register(
                bot_ids=["greeting-bot"],
                capabilities=["ReadMessages"],
                event_types=["dm_received"],
                timeout=5.0,
            )
        )
        await asyncio.sleep(0.05)
        request = mock_transport.frames[0]
        mock_transport.inject(
            {
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "handler_id": "h-1",
                    "reconnect_token": "rt-1",
                    "registered_events": ["dm_received"],
                },
            }
        )
        result = await task
        assert result.handler_id == "h-1"
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_call_timeout_none_disables_default(mock_transport):
    """Passing timeout=None on a call disables the default timeout."""
    client = PactoClient(mock_transport, timeout=0.01)
    await client.connect()
    try:
        task = asyncio.create_task(
            client.handler_register(
                bot_ids=["greeting-bot"],
                capabilities=["ReadMessages"],
                event_types=["dm_received"],
                timeout=None,
            )
        )
        await asyncio.sleep(0.05)
        request = mock_transport.frames[0]
        mock_transport.inject(
            {
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "handler_id": "h-1",
                    "reconnect_token": "rt-1",
                    "registered_events": ["dm_received"],
                },
            }
        )
        result = await task
        assert result.handler_id == "h-1"
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_request_timeout_raises_pacto_client_error(mock_transport):
    """A request that exceeds its timeout raises ``PactoClientError``."""
    client = PactoClient(mock_transport, timeout=0.01)
    await client.connect()
    try:
        with pytest.raises(PactoClientError, match="timed out"):
            await client.handler_register(
                bot_ids=["greeting-bot"],
                capabilities=["ReadMessages"],
                event_types=["dm_received"],
            )
    finally:
        await client.close()

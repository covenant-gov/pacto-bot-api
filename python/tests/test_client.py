"""Tests for the generated low-level async client."""

from __future__ import annotations

import asyncio

import pytest

from pacto_bot_sdk import (
    AgentEventParams,
    HandlerRegisterParams,
    HandlerRegisterResult,
    PactoClientError,
)


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
    assert isinstance(result, HandlerRegisterResult)
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
    result = HandlerRegisterResult(
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

    with pytest.raises(PactoClientError, match="Invalid Request"):
        await task


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

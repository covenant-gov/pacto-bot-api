"""Tests for the pacto-bot-api high-level Bot layer."""

from __future__ import annotations

import asyncio
import json
from typing import Any

import pytest

from pacto_bot_api import Bot, PactoClient, parse_command
from pacto_bot_api.transports import Transport, UnixTransport


# ---------------------------------------------------------------------------
# Mock transport
# ---------------------------------------------------------------------------


class MockTransport:
    """In-memory transport for driving Bot in tests."""

    name = "mock"

    def __init__(self) -> None:
        self.frames: list[dict[str, Any]] = []
        self._inbound: asyncio.Queue[str] = asyncio.Queue()
        self.connected = False
        self.closed = False
        self.handler_id: str | None = None

    async def connect(self) -> None:
        self.connected = True

    async def close(self) -> None:
        self.closed = True

    async def readline(self) -> str:
        return await self._inbound.get()

    async def write_frame(self, frame: dict[str, Any]) -> dict[str, Any] | None:
        self.frames.append(frame)
        return None

    def inject(self, frame: dict[str, Any]) -> None:
        self._inbound.put_nowait(json.dumps(frame))


@pytest.fixture
def transport() -> MockTransport:
    return MockTransport()


@pytest.fixture
def bot(transport: MockTransport) -> Bot:
    return Bot(
        bot_id="test-bot",
        transport=transport,
        event_types=["dm_received"],
        capabilities=["ReadMessages", "SendMessages"],
    )


# ---------------------------------------------------------------------------
# Command dispatch
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_command_handler_receives_parsed_args_and_sends_response(bot, transport):
    calls: list[tuple[Any, Bot]] = []

    @bot.command("/hello")
    async def hello(event, b):
        calls.append((event, b))
        return {
            "event_id": event.event_id,
            "action": "reply",
            "content": f"Hello {event.content}!",
        }

    task = asyncio.create_task(bot._run(["--transport", "mock"]))

    # Wait for registration frame to be sent and inject response.
    await asyncio.sleep(0.05)
    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "registered_events": ["dm_received"]},
            })
            break

    # Wait for registration to complete.
    await asyncio.sleep(0.05)

    # Inject an incoming dm_received event.
    transport.inject({
        "jsonrpc": "2.0",
        "method": "agent.event",
        "params": {
            "bot_id": "test-bot",
            "event_id": "e-1",
            "type": "dm_received",
            "chat_id": "npub1chat",
            "content": "/hello world --shout",
            "rumor_id": "r-1",
            "author": "npub1author",
            "timestamp": 1234567890,
        },
    })

    await asyncio.sleep(0.05)

    # Find the handler.response frame.
    responses = [f for f in transport.frames if f.get("method") == "handler.response"]
    assert len(responses) == 1
    assert responses[0]["params"] == {
        "event_id": "e-1",
        "action": "reply",
        "content": "Hello /hello world --shout!",
    }

    assert len(calls) == 1
    event, called_bot = calls[0]
    assert called_bot is bot
    assert event.event_id == "e-1"
    assert event.content == "/hello world --shout"

    bot._request_shutdown()
    await task


@pytest.mark.asyncio
async def test_unknown_command_routes_to_default(bot, transport):
    @bot.default
    async def fallback(event, b):
        return {
            "event_id": event.event_id,
            "action": "reply",
            "content": "I don't know that command.",
        }

    task = asyncio.create_task(bot._run(["--transport", "mock"]))
    await asyncio.sleep(0.05)

    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "registered_events": ["dm_received"]},
            })
            break

    await asyncio.sleep(0.05)

    transport.inject({
        "jsonrpc": "2.0",
        "method": "agent.event",
        "params": {
            "bot_id": "test-bot",
            "event_id": "e-2",
            "type": "dm_received",
            "chat_id": "npub1chat",
            "content": "/unknown",
            "rumor_id": "r-2",
            "author": "npub1author",
            "timestamp": 1234567890,
        },
    })

    await asyncio.sleep(0.05)

    responses = [f for f in transport.frames if f.get("method") == "handler.response"]
    assert len(responses) == 1
    assert responses[0]["params"]["action"] == "reply"
    assert "don't know" in responses[0]["params"]["content"]

    bot._request_shutdown()
    await task


@pytest.mark.asyncio
async def test_unknown_command_without_default_ignores(bot, transport):
    task = asyncio.create_task(bot._run(["--transport", "mock"]))
    await asyncio.sleep(0.05)

    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "registered_events": ["dm_received"]},
            })
            break

    await asyncio.sleep(0.05)

    transport.inject({
        "jsonrpc": "2.0",
        "method": "agent.event",
        "params": {
            "bot_id": "test-bot",
            "event_id": "e-3",
            "type": "dm_received",
            "chat_id": "npub1chat",
            "content": "/unknown",
            "rumor_id": "r-3",
            "author": "npub1author",
            "timestamp": 1234567890,
        },
    })

    await asyncio.sleep(0.05)

    responses = [f for f in transport.frames if f.get("method") == "handler.response"]
    assert len(responses) == 1
    assert responses[0]["params"] == {"event_id": "e-3", "action": "ignore"}

    bot._request_shutdown()
    await task


def test_parse_command_is_exported():
    """parse_command is available from the top-level package."""
    assert parse_command("/hello world") == {
        "command": "hello",
        "args": ["world"],
        "flags": {},
    }


@pytest.mark.asyncio
async def test_handler_exception_replies_with_friendly_error_by_default(
    transport: MockTransport,
) -> None:
    bot = Bot("test-bot", transport=transport)

    @bot.command("/boom")
    async def boom(_event, _b):
        raise RuntimeError("intentional failure")

    task = asyncio.create_task(bot._run(["--transport", "mock"]))
    await asyncio.sleep(0.05)

    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "registered_events": ["dm_received"]},
            })
            break

    await asyncio.sleep(0.05)

    transport.inject({
        "jsonrpc": "2.0",
        "method": "agent.event",
        "params": {
            "bot_id": "test-bot",
            "event_id": "e-boom",
            "type": "dm_received",
            "chat_id": "npub1chat",
            "content": "/boom",
            "rumor_id": "r-boom",
            "author": "npub1author",
            "timestamp": 1234567890,
        },
    })

    await asyncio.sleep(0.05)

    responses = [f for f in transport.frames if f.get("method") == "handler.response"]
    assert len(responses) == 1
    assert responses[0]["params"]["action"] == "reply"
    assert responses[0]["params"]["event_id"] == "e-boom"
    assert responses[0]["params"]["content"] == "Sorry, I couldn't process that."
    assert "RuntimeError" not in str(responses[0]["params"])

    bot._request_shutdown()
    await task


@pytest.mark.asyncio
async def test_handler_exception_is_silent_when_reply_on_error_disabled(
    transport: MockTransport,
) -> None:
    bot = Bot("test-bot", transport=transport, reply_on_error=False)

    @bot.command("/boom")
    async def boom(_event, _b):
        raise RuntimeError("intentional failure")

    task = asyncio.create_task(bot._run(["--transport", "mock"]))
    await asyncio.sleep(0.05)

    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "registered_events": ["dm_received"]},
            })
            break

    await asyncio.sleep(0.05)

    transport.inject({
        "jsonrpc": "2.0",
        "method": "agent.event",
        "params": {
            "bot_id": "test-bot",
            "event_id": "e-boom",
            "type": "dm_received",
            "chat_id": "npub1chat",
            "content": "/boom",
            "rumor_id": "r-boom",
            "author": "npub1author",
            "timestamp": 1234567890,
        },
    })

    await asyncio.sleep(0.05)

    responses = [f for f in transport.frames if f.get("method") == "handler.response"]
    assert len(responses) == 1
    assert responses[0]["params"] == {"event_id": "e-boom", "action": "ignore"}

    bot._request_shutdown()
    await task


# ---------------------------------------------------------------------------
# Registration
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_registration_sends_correct_capabilities(bot, transport):
    task = asyncio.create_task(bot._run(["--transport", "mock"]))
    await asyncio.sleep(0.05)

    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "registered_events": ["dm_received"]},
            })
            break

    await asyncio.sleep(0.05)

    register_frames = [f for f in transport.frames if f.get("method") == "handler.register"]
    assert len(register_frames) == 1
    params = register_frames[0]["params"]
    assert params["bot_ids"] == ["test-bot"]
    assert params["event_types"] == ["dm_received"]
    assert params["capabilities"] == ["ReadMessages", "SendMessages"]

    bot._request_shutdown()
    await task


# ---------------------------------------------------------------------------
# Status handler
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_status_handler_called(bot, transport):
    statuses: list[Any] = []

    @bot.status
    async def on_status(status, b):
        statuses.append(status)

    task = asyncio.create_task(bot._run(["--transport", "mock"]))
    await asyncio.sleep(0.05)

    for frame in transport.frames:
        if frame.get("method") == "handler.register":
            transport.inject({
                "jsonrpc": "2.0",
                "id": frame["id"],
                "result": {"handler_id": "h-1", "registered_events": ["dm_received"]},
            })
            break

    await asyncio.sleep(0.05)

    transport.inject({
        "jsonrpc": "2.0",
        "method": "agent.status",
        "params": {"state": "ready", "capabilities": ["ReadMessages"]},
    })

    await asyncio.sleep(0.05)

    assert len(statuses) == 1
    assert statuses[0].state == "ready"

    bot._request_shutdown()
    await task


# ---------------------------------------------------------------------------
# Constructor transport resolution
# ---------------------------------------------------------------------------


def test_bot_accepts_transport_instance(bot, transport):
    assert bot._transport is transport


def test_bot_rejects_unknown_transport_name():
    with pytest.raises(ValueError, match="unsupported transport"):
        Bot("x", transport="udp")


@pytest.mark.asyncio
async def test_cli_transport_overrides_constructor_string():
    """CLI --transport overrides a constructor string transport."""
    bot = Bot("x", transport="http", secret="secret")
    # Parsing argv should switch the resolved transport to unix.
    args = bot._parse_args(["--transport", "unix"])
    assert args.transport == "unix"

    # _make_transport should reflect the CLI override when not given an instance.
    transport = bot._make_transport(
        args.transport, None, None, None, data_dir=bot._data_dir
    )
    assert isinstance(transport, UnixTransport)
